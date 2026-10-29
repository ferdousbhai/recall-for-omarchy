//! In-process Office archives, bounded metadata, and incremental XML text/rows.
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use quick_xml::{Reader, Writer, events::Event};
use zip::{ZipArchive, read::ZipFile};

use crate::tools::{ExtractError, Result};

pub(super) const MAX_TEXT: usize = 64 * 1024 * 1024;
const MAX_METADATA: u64 = 8 * 1024 * 1024;
const MAX_XML: u64 = 64 * 1024 * 1024;
const MAX_ELEMENT: usize = 1024 * 1024;

fn unreadable(error: impl std::fmt::Display) -> ExtractError {
    ExtractError::Unreadable(format!("Office document: {error}"))
}

pub(super) struct Archive(ZipArchive<File>);

impl Archive {
    pub(super) fn open(path: &Path) -> Result<Self> {
        Ok(Self(ZipArchive::new(File::open(path).map_err(unreadable)?).map_err(unreadable)?))
    }

    pub(super) fn members(&self) -> Result<Vec<(String, u64)>> {
        (0..self.0.len()).map(|index| {
            let file = self.0.by_index_data(index).map_err(unreadable)?;
            Ok((file.name().map_err(unreadable)?.into_owned(), file.size()))
        }).collect()
    }

    pub(super) fn member(&mut self, name: &str) -> Result<ZipFile<'_, File>> {
        self.0.by_name(name).map_err(unreadable)
    }

    pub(super) fn xml(&mut self, name: &str) -> Result<String> {
        let member = self.member(name)?;
        if member.size() > MAX_METADATA {
            return Err(unreadable("XML metadata exceeds 8 MiB"));
        }
        let mut bytes = Vec::new();
        member.take(MAX_METADATA + 1).read_to_end(&mut bytes).map_err(unreadable)?;
        if bytes.is_empty() || bytes.len() as u64 > MAX_METADATA {
            return Err(unreadable("empty or oversized XML metadata"));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// A row or shared string at a time, without holding the decompressed worksheet.
pub(super) struct Elements<R: Read> {
    reader: Reader<BufReader<std::io::Take<R>>>,
    buffer: Vec<u8>,
    tag: &'static [u8],
    done: bool,
}

impl<R: Read> Elements<R> {
    pub(super) fn new(reader: R, tag: &'static [u8]) -> Self {
        Self { reader: Reader::from_reader(BufReader::new(reader.take(MAX_XML + 1))), buffer: Vec::new(), tag, done: false }
    }

    fn next_element(&mut self) -> Result<Option<String>> {
        let mut writer = Writer::new(Vec::new());
        let mut depth = 0;
        loop {
            self.buffer.clear();
            let event = self.reader.read_event_into(&mut self.buffer).map_err(unreadable)?;
            if self.reader.buffer_position() > MAX_XML {
                return Err(unreadable("XML scan exceeds 64 MiB"));
            }
            match &event {
                Event::Start(tag) if depth > 0 || tag.name().as_ref() == self.tag => depth += 1,
                Event::Empty(tag) if depth == 0 && tag.name().as_ref() == self.tag => {
                    writer.write_event(event).map_err(unreadable)?;
                    return Ok(Some(String::from_utf8_lossy(&writer.into_inner()).into_owned()));
                }
                Event::Eof if depth > 0 => return Err(unreadable("unfinished XML element")),
                Event::Eof => return Ok(None),
                _ => {}
            }
            if depth > 0 {
                let end = matches!(event, Event::End(_));
                if writer.get_ref().len().saturating_add(event.as_ref().len() + 5) > MAX_ELEMENT {
                    return Err(unreadable("XML row or shared string exceeds 1 MiB"));
                }
                writer.write_event(event).map_err(unreadable)?;
                if end {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(Some(String::from_utf8_lossy(&writer.into_inner()).into_owned()));
                    }
                }
            }
        }
    }
}

impl<R: Read> Iterator for Elements<R> {
    type Item = Result<String>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.next_element() {
            Ok(Some(xml)) => Some(Ok(xml)),
            Ok(None) => { self.done = true; None }
            Err(error) => { self.done = true; Some(Err(error)) }
        }
    }
}

/// Word-processing paragraphs and slide text, with one XML token in memory.
pub(super) fn text(input: impl Read, paragraph: &str, tab: &str, br: &str, space: Option<&str>, notes: bool) -> Result<String> {
    let mut reader = Reader::from_reader(BufReader::new(input.take(MAX_XML + 1)));
    let mut buffer = Vec::new();
    let mut out = String::new();
    let mut skipped = 0;
    loop {
        buffer.clear();
        let event = reader.read_event_into(&mut buffer).map_err(unreadable)?;
        if reader.buffer_position() > MAX_XML {
            return Err(unreadable("XML scan exceeds 64 MiB"));
        }
        match event {
            Event::Start(tag) => {
                if skipped > 0 || (notes && tag.name().as_ref() == b"a:fld") {
                    skipped += 1;
                }
            }
            Event::End(_) if skipped > 0 => skipped -= 1,
            Event::End(tag) if paragraph.split('|').any(|p| p.as_bytes() == tag.name().as_ref()) => out.push('\n'),
            Event::Empty(tag) if skipped == 0 => {
                let name = tag.name();
                if paragraph.split('|').any(|p| p.as_bytes() == name.as_ref()) || br.as_bytes() == name.as_ref() {
                    out.push('\n');
                } else if tab.as_bytes() == name.as_ref() {
                    out.push('\t');
                } else if space.is_some_and(|s| s.as_bytes() == name.as_ref()) {
                    out.push(' ');
                }
            }
            Event::Text(text) if skipped == 0 => out.push_str(&text.decode().map_err(unreadable)?),
            Event::CData(text) if skipped == 0 => out.push_str(&text.decode().map_err(unreadable)?),
            Event::GeneralRef(reference) if skipped == 0 => {
                out.push_str(&super::unescape(&format!("&{};", reference.decode().map_err(unreadable)?)));
            }
            Event::Eof => break,
            _ => {}
        }
        if out.len() >= MAX_TEXT {
            truncate(&mut out, MAX_TEXT);
            break;
        }
    }
    Ok(out.trim().to_string())
}

pub(super) fn truncate(text: &mut String, mut limit: usize) {
    if text.len() > limit {
        while !text.is_char_boundary(limit) { limit -= 1; }
        text.truncate(limit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Counted<'a> {
        input: &'a [u8],
        bytes: &'a Cell<usize>,
    }

    impl Read for Counted<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let count = self.input.read(buffer)?;
            self.bytes.set(self.bytes.get() + count);
            Ok(count)
        }
    }

    #[test]
    fn streaming_rows_stop_reading_when_the_output_is_full() {
        let xml = format!("<worksheet><sheetData>{}</sheetData></worksheet>", "<row><c><v>1234567890</v></c></row>".repeat(100_000));
        let bytes = Cell::new(0);
        let input = Counted { input: xml.as_bytes(), bytes: &bytes };
        let rows = Elements::new(input, b"row").map(|row| {
            row.unwrap();
            vec![(0, "1234567890".to_string())]
        });
        let mut out = String::new();
        assert!(super::super::render_sheet(&mut out, "Numbers", rows, 40));
        assert!(bytes.get() <= 8192, "read {} of {} bytes", bytes.get(), xml.len());
    }

    #[test]
    fn an_oversized_or_unfinished_row_fails_once() {
        let xml = format!("<row><c><v>{}</v></c></row>", "x".repeat(MAX_ELEMENT));
        let mut rows = Elements::new(xml.as_bytes(), b"row");
        assert!(rows.next().unwrap().unwrap_err().to_string().contains("1 MiB"));
        assert!(rows.next().is_none());
        let mut rows = Elements::new("<row><c>unfinished".as_bytes(), b"row");
        assert!(rows.next().unwrap().is_err());
        assert!(rows.next().is_none());
    }

    #[test]
    fn streaming_text_matches_paragraphs_entities_tabs_and_notes() {
        let xml = concat!(
            "<w:document><w:p><w:t>Pets &amp; rent &#233; &#xE9; &amp;lt;</w:t></w:p>",
            "<w:p>One<w:tab/>cat<w:br/>today</w:p><w:p/></w:document>"
        );
        assert_eq!(text(xml.as_bytes(), "w:p", "w:tab", "w:br", None, false).unwrap(),
                   super::super::xml_text(xml, "w:p", "w:tab", "w:br", None));
        let notes = "<p:notes><a:p>Keep <a:fld><a:r><a:t>27</a:t></a:r></a:fld>notes</a:p></p:notes>";
        assert_eq!(text(notes.as_bytes(), "a:p", "a:tab", "a:br", None, true).unwrap(), "Keep notes");
        let odt = "<office:text><text:h>Lease</text:h><text:p>A<text:s/>b<text:line-break/>c</text:p></office:text>";
        assert_eq!(text(odt.as_bytes(), "text:p|text:h", "text:tab", "text:line-break", Some("text:s"), false).unwrap(),
                   super::super::xml_text(odt, "text:p|text:h", "text:tab", "text:line-break", Some("text:s")));
        let empty: Vec<_> = Elements::new("<sst><si/><si><t>next</t></si></sst>".as_bytes(), b"si").collect();
        assert_eq!(empty.len(), 2);
        assert_eq!(empty[0].as_ref().unwrap(), "<si/>");
    }
}
