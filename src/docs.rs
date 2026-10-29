//! Text from documents: txt and md as they are, HTML without its tags, docx and odt from their zipped XML, slide decks
//! (pptx) a slide at a time with their speaker notes, spreadsheets (xlsx) a row at a time under their column headers,
//! RTF read here, old Word files through catdoc or antiword; and the text cut into overlapping chunks for the model.

use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use unicode_segmentation::UnicodeSegmentation;

mod office;
use office::{Archive, Elements};

use crate::rules;
use crate::tools::{self, ExtractError, Result};

pub fn text_of(path: &Path) -> Result<String> {
    match rules::extension(path).as_str() {
        "docx" => office::text(Archive::open(path)?.member("word/document.xml")?, "w:p", "w:tab", "w:br", None, false),
        "odt" => office::text(Archive::open(path)?.member("content.xml")?, "text:p|text:h", "text:tab", "text:line-break", Some("text:s"), false),
        "rtf" => Ok(rtf(&std::fs::read(path).map_err(|e| ExtractError::Unreadable(e.to_string()))?)),
        "pptx" => slides(path),
        "xlsx" => spreadsheet(path),
        "doc" => old_word(path),
        "html" | "htm" => Ok(plain_text_from_html(&read_text(path)?)),
        _ => read_text(path),
    }
}

/// A text file in UTF-8 or UTF-16 (with its byte order mark), else Latin-1.
pub fn read_text(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).map_err(|error| ExtractError::Unreadable(error.to_string()))?;
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return Ok(String::from_utf8_lossy(rest).into_owned());
    }
    let utf16 = |rest: &[u8], little: bool| {
        let units: Vec<u16> = rest
            .as_chunks::<2>().0.iter()
            .map(|pair| if little { u16::from_le_bytes(*pair) } else { u16::from_be_bytes(*pair) })
            .collect();
        String::from_utf16_lossy(&units)
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return Ok(utf16(rest, true));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return Ok(utf16(rest, false));
    }
    match String::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(error) => Ok(error.into_bytes().iter().map(|byte| *byte as char).collect()),
    }
}

fn old_word(path: &Path) -> Result<String> {
    let file = path.to_string_lossy().into_owned();
    for (tool, arguments) in [("catdoc", vec!["-d", "utf-8", "-w", file.as_str()]), ("antiword", vec!["-w", "0", file.as_str()])] {
        if tools::path(tool).is_some() {
            let read = tools::run(tool, &arguments)?;
            if !read.ok() {
                return Err(ExtractError::Unreadable(format!("{tool}: {}", read.complaint())));
            }
            return Ok(read.text());
        }
    }
    Err(ExtractError::Unreadable("old Word files need catdoc or antiword".into()))
}

/// A zipped document's members, read from its central directory without extracting them.
pub fn zip_members(path: &Path) -> Result<Vec<(String, u64)>> {
    Archive::open(path)?.members()
}

/// About how many characters of text a zipped document holds, from the size of the XML that has its words (the rest is
/// pictures and styles): for estimates, which can't go by the file's size.
pub fn zipped_text_size(path: &Path) -> Option<u64> {
    let text = |name: &str| match rules::extension(path).as_str() {
        "docx" => name == "word/document.xml",
        "odt" => name == "content.xml",
        "pptx" => name.starts_with("ppt/slides/slide") || name.starts_with("ppt/notesSlides/notesSlide"),
        "xlsx" => name == "xl/sharedStrings.xml" || name.starts_with("xl/worksheets/sheet"),
        _ => false,
    };
    let bytes: u64 = zip_members(path).ok()?.iter().filter(|(name, _)| text(name)).map(|(_, size)| size).sum();
    Some(bytes / 6) // the markup around each word is most of it
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    static ATTRIBUTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\s([^\s=]+)="([^"]*)""#).unwrap());
    ATTRIBUTE.captures_iter(tag).find(|found| &found[1] == name).map(|found| unescape(&found[2]))
}

fn unescape(text: &str) -> String {
    let mut text = text.to_string();
    for (entity, character) in [("&lt;", "<"), ("&gt;", ">"), ("&quot;", "\""), ("&apos;", "'")] {
        text = text.replace(entity, character);
    }
    decode_numeric_entities(&text.replace("&amp;", "&"))
}

/// A part's relationships (`_rels/<part>.rels`): id → the member it points at.
fn relationships(archive: &mut Archive, part: &str) -> std::collections::HashMap<String, (String, String)> {
    static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<Relationship\b[^>]*>").unwrap());
    let (folder, name) = part.rsplit_once('/').unwrap_or(("", part));
    let rels = if folder.is_empty() { format!("_rels/{name}.rels") } else { format!("{folder}/_rels/{name}.rels") };
    let Ok(xml) = archive.xml(&rels) else { return Default::default() };
    TAG.find_iter(&xml)
        .filter_map(|tag| {
            let tag = tag.as_str();
            let target = attribute(tag, "Target")?;
            Some((attribute(tag, "Id")?, (resolve(folder, &target), attribute(tag, "Type").unwrap_or_default())))
        })
        .collect()
}

/// `target` relative to the folder `base` (or from the root, with a leading slash), with `..` taken out.
fn resolve(base: &str, target: &str) -> String {
    let mut parts: Vec<&str> = if target.starts_with('/') { Vec::new() } else { base.split('/').filter(|p| !p.is_empty()).collect() };
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}

/// The members named `prefix<n>.xml`, by n.
fn numbered(members: &[(String, u64)], prefix: &str) -> Vec<String> {
    let mut found: Vec<(u64, String)> = members
        .iter()
        .filter_map(|(name, _)| {
            let n = name.strip_prefix(prefix)?.strip_suffix(".xml")?.parse().ok()?;
            Some((n, name.clone()))
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, name)| name).collect()
}

// MARK: Slides

/// A deck's slides in the order they're shown, each with its speaker notes: "Slide 3", its text a paragraph a line,
/// then "Notes: …".
fn slides(path: &Path) -> Result<String> {
    static SLIDE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<p:sldId\b[^>]*>").unwrap());
    let mut archive = Archive::open(path)?;
    let presentation = archive.xml("ppt/presentation.xml")?;
    let rels = relationships(&mut archive, "ppt/presentation.xml");
    let mut order: Vec<String> = SLIDE_ID
        .find_iter(&presentation)
        .filter_map(|tag| attribute(tag.as_str(), "r:id"))
        .filter_map(|id| rels.get(&id).map(|(member, _)| member.clone()))
        .collect();
    if order.is_empty() {
        order = numbered(&archive.members()?, "ppt/slides/slide");
    }
    let mut out = String::new();
    for (index, member) in order.iter().enumerate() {
        let Ok(input) = archive.member(member) else { continue };
        let text = office::text(input, "a:p", "a:tab", "a:br", None, false)?;
        // The notes, without the slide number field their page carries.
        let notes_member = relationships(&mut archive, member).into_values()
            .find(|(_, kind)| kind.ends_with("/notesSlide"));
        let notes = match notes_member.and_then(|(notes, _)| archive.member(&notes).ok()) {
            Some(input) => office::text(input, "a:p", "a:tab", "a:br", None, true)?,
            None => String::new(),
        };
        if text.is_empty() && notes.is_empty() {
            continue;
        }
        out += &format!("Slide {}\n", index + 1);
        if !text.is_empty() {
            out += &format!("{text}\n");
        }
        if !notes.is_empty() {
            out += &format!("Notes: {notes}\n");
        }
        out.push('\n');
        if out.len() > office::MAX_TEXT {
            return Err(ExtractError::Unreadable("Office slide text exceeds 64 MiB".into()));
        }
    }
    Ok(out.trim_end().to_string())
}

// MARK: Spreadsheets

/// Text past this is left out: a sheet of a million rows is data, and the chunks would stop at the cap anyway.
const MAX_SHEET_TEXT: usize = 8_000_000;

/// A workbook's visible sheets: "Sheet: Name", then each row as "Header: value · Header: value" under the first row's
/// column headers (when the first row looks like headers), so a row reads as a sentence about one thing. Dates are
/// written as dates.
fn spreadsheet(path: &Path) -> Result<String> {
    static SHEET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<sheet\b[^>]*>").unwrap());
    static CELL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<c\b([^>]*?)(?:/>|>(.*?)</c>)").unwrap());
    static VALUE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<v>(.*?)</v>").unwrap());
    let mut archive = Archive::open(path)?;
    let workbook = archive.xml("xl/workbook.xml")?;
    let rels = relationships(&mut archive, "xl/workbook.xml");
    let shared = shared_strings(&mut archive)?;
    let dates = date_styles(&mut archive);
    let mut out = String::new();
    for tag in SHEET.find_iter(&workbook) {
        let tag = tag.as_str();
        if attribute(tag, "state").is_some_and(|state| state != "visible") {
            continue;
        }
        let Some((member, _)) = attribute(tag, "r:id").and_then(|id| rels.get(&id).cloned()) else { continue };
        let Ok(input) = archive.member(&member) else { continue };
        let mut failure = None;
        let rows = Elements::new(input, b"row").map_while(|xml| match xml {
            Ok(xml) => Some(xml),
            Err(error) => { failure = Some(error); None }
        }).filter_map(|row| {
            let mut cells = Vec::new();
            for (position, cell) in CELL.captures_iter(&row).enumerate() {
                let attributes = &cell[1];
                let body = cell.get(2).map_or("", |body| body.as_str());
                let column = attribute(attributes, "r").and_then(|place| column_of(&place)).unwrap_or(position);
                let raw = VALUE.captures(body).map(|value| unescape(&value[1]));
                let value = match attribute(attributes, "t").as_deref() {
                    Some("s") => raw.and_then(|index| index.parse::<usize>().ok()).and_then(|index| shared.get(index).cloned()),
                    Some("inlineStr") => Some(run_text(body)),
                    Some("b") => raw.map(|value| if value == "1" { "TRUE".into() } else { "FALSE".into() }),
                    Some("e") => None,
                    Some("str") => raw,
                    _ => raw.map(|value| {
                        let style = attribute(attributes, "s").and_then(|s| s.parse::<usize>().ok());
                        match (style.is_some_and(|style| dates.contains(&style)), value.parse::<f64>()) {
                            (true, Ok(serial)) => date(serial),
                            _ => value,
                        }
                    }),
                };
                if let Some(value) = value.map(|value| value.split_whitespace().collect::<Vec<_>>().join(" ")) {
                    if !value.is_empty() {
                        cells.push((column, value));
                    }
                }
            }
            (!cells.is_empty()).then_some(cells)
        });
        let name = attribute(tag, "name").unwrap_or_default();
        let capped = render_sheet(&mut out, &name, rows, MAX_SHEET_TEXT);
        if let Some(error) = failure { return Err(error); }
        if capped { return Ok(out); }
    }
    Ok(out.trim_end().to_string())
}

/// Writes a sheet's rows (under its header row, when the first row is words and another follows); stops pulling rows
/// once `out` passes `cap`, returning whether it did.
fn render_sheet(out: &mut String, name: &str, rows: impl Iterator<Item = Vec<(usize, String)>>, cap: usize) -> bool {
    let mut rows = rows.peekable();
    let Some(first) = rows.next() else { return false };
    *out += &format!("Sheet: {name}\n");
    let wordy = first.iter().filter(|(_, value)| value.parse::<f64>().is_err()).count();
    let headers: std::collections::HashMap<usize, String> =
        if wordy * 2 > first.len() && rows.peek().is_some() { first.iter().cloned().collect() } else { Default::default() };
    if !headers.is_empty() {
        *out += &format!("Columns: {}\n", first.iter().map(|(_, value)| value.as_str()).collect::<Vec<_>>().join(" · "));
    }
    let first = headers.is_empty().then_some(first);
    for row in first.into_iter().chain(rows) {
        let line: Vec<String> = row.iter().map(|(column, value)| match headers.get(column) {
            Some(header) => format!("{header}: {value}"),
            None => value.clone(),
        }).collect();
        *out += &line.join(" · ");
        out.push('\n');
        if out.len() > cap {
            return true;
        }
    }
    out.push('\n');
    false
}

/// The text of a cell's or a shared string's runs (`<t>`), without the phonetic guides some languages add.
fn run_text(xml: &str) -> String {
    static PHONETIC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<rPh\b.*?</rPh>").unwrap());
    static TEXT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<t\b[^>]*>(.*?)</t>").unwrap());
    let xml = PHONETIC.replace_all(xml, "");
    unescape(&TEXT.captures_iter(&xml).map(|found| found[1].to_string()).collect::<String>())
}

fn shared_strings(archive: &mut Archive) -> Result<Vec<String>> {
    let Ok(input) = archive.member("xl/sharedStrings.xml") else { return Ok(Vec::new()) };
    let mut strings = Vec::new();
    let mut bytes = 0;
    for xml in Elements::new(input, b"si") {
        let text = run_text(&xml?);
        bytes += text.len();
        if bytes > 16 * 1024 * 1024 || strings.len() >= 250_000 {
            return Err(ExtractError::Unreadable("Office shared strings exceed 16 MiB or 250000 entries".into()));
        }
        strings.push(text);
    }
    Ok(strings)
}

/// The cell styles (by index) that show a number as a date: Excel's own date formats (14–22, 45–47) and custom ones
/// with a day or a year in them.
fn date_styles(archive: &mut Archive) -> HashSet<usize> {
    static FORMAT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<numFmt\b[^>]*>").unwrap());
    static STYLES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<cellXfs\b[^>]*>(.*?)</cellXfs>").unwrap());
    static STYLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<xf\b[^>]*>").unwrap());
    static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""[^"]*"|\[[^\]]*\]|\\."#).unwrap());
    let Ok(xml) = archive.xml("xl/styles.xml") else { return HashSet::new() };
    let mut formats: HashSet<u32> = (14..=22).chain(45..=47).collect();
    for tag in FORMAT.find_iter(&xml) {
        let (Some(id), Some(code)) = (attribute(tag.as_str(), "numFmtId"), attribute(tag.as_str(), "formatCode")) else {
            continue;
        };
        let code = QUOTED.replace_all(&code, "").to_lowercase();
        if code.contains('d') || code.contains('y') {
            if let Ok(id) = id.parse() {
                formats.insert(id);
            }
        }
    }
    let Some(styles) = STYLES.captures(&xml) else { return HashSet::new() };
    STYLE
        .find_iter(&styles[1])
        .enumerate()
        .filter(|(_, tag)| {
            attribute(tag.as_str(), "numFmtId").and_then(|id| id.parse().ok()).is_some_and(|id: u32| formats.contains(&id))
        })
        .map(|(index, _)| index)
        .collect()
}

/// "B12" → 1 (columns from 0).
fn column_of(place: &str) -> Option<usize> {
    let letters: String = place.chars().take_while(char::is_ascii_alphabetic).collect();
    if letters.is_empty() {
        return None;
    }
    Some(letters.to_ascii_uppercase().bytes().fold(0, |column, letter| column * 26 + (letter - b'A' + 1) as usize) - 1)
}

/// A spreadsheet's date number (days since 30 December 1899) as "2026-03-14", with the time when it has one.
fn date(serial: f64) -> String {
    let days = serial.floor() as i64 - 25569; // to days since 1970
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let seconds = ((serial - serial.floor()) * 86_400.0).round() as i64;
    if seconds == 0 {
        format!("{year:04}-{month:02}-{day:02}")
    } else {
        format!("{year:04}-{month:02}-{day:02} {:02}:{:02}", seconds / 3600, seconds / 60 % 60)
    }
}

static NUMERIC_ENTITY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&#(x[0-9a-fA-F]+|[0-9]+);").unwrap());

pub fn decode_numeric_entities(text: &str) -> String {
    if !text.contains("&#") {
        return text.to_string();
    }
    NUMERIC_ENTITY
        .replace_all(text, |captures: &regex::Captures| {
            let code = &captures[1];
            let value = match code.strip_prefix('x') {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => code.parse().ok(),
            };
            value.and_then(char::from_u32).map(String::from).unwrap_or_else(|| " ".into())
        })
        .into_owned()
}

/// The text of a word processor's XML: paragraphs become lines, tags go, entities are decoded.
pub fn xml_text(xml: &str, paragraph: &str, tab: &str, line_break: &str, space: Option<&str>) -> String {
    let replace = |text: &str, pattern: &str, with: &str| Regex::new(pattern).unwrap().replace_all(text, with).into_owned();
    let mut text = replace(xml, &format!(r"<({paragraph})\b[^>]*/>"), "\n");
    text = replace(&text, &format!("</({paragraph})>"), "\n");
    text = replace(&text, &format!(r"<{tab}\b[^>]*/>"), "\t");
    text = replace(&text, &format!(r"<{line_break}\b[^>]*/>"), "\n");
    if let Some(space) = space {
        text = replace(&text, &format!(r"<{space}\b[^>]*/>"), " ");
    }
    text = replace(&text, "<[^>]+>", "");
    for (entity, character) in [("&lt;", "<"), ("&gt;", ">"), ("&quot;", "\""), ("&apos;", "'")] {
        text = text.replace(entity, character);
    }
    decode_numeric_entities(&text.replace("&amp;", "&")).trim().to_string()
}

static HTML_DROPS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [r"(?is)<script\b.*?</script\s*>", r"(?is)<style\b.*?</style\s*>", r"(?s)<!--.*?-->", r"(?is)<head\b.*?</head\s*>"]
        .iter()
        .map(|pattern| Regex::new(pattern).unwrap())
        .collect()
});
static HTML_BREAKS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<(br|/p|/div|/li|/h[1-6]|/tr|/section|/article)\b[^>]*>").unwrap());
static TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
static BLANKS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]+").unwrap());
static LINE_BREAKS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\n\s*").unwrap());

/// Good enough for search: drops scripts, styles and comments, turns block ends into line breaks, strips the remaining
/// tags, and decodes common entities.
pub fn plain_text_from_html(html: &str) -> String {
    let mut text = html.to_string();
    for pattern in HTML_DROPS.iter() {
        text = pattern.replace_all(&text, " ").into_owned();
    }
    text = HTML_BREAKS.replace_all(&text, "\n").into_owned();
    text = TAGS.replace_all(&text, " ").into_owned();
    for (entity, character) in [
        ("&nbsp;", " "), ("&amp;", "&"), ("&lt;", "<"), ("&gt;", ">"), ("&quot;", "\""), ("&#39;", "'"), ("&apos;", "'"),
        ("&mdash;", "—"), ("&ndash;", "–"), ("&hellip;", "…"),
    ] {
        text = text.replace(entity, character);
    }
    text = decode_numeric_entities(&text);
    let text = BLANKS.replace_all(&text, " ");
    LINE_BREAKS.replace_all(&text, "\n").trim().to_string()
}

static CHUNK_BLANKS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t\x{00A0}]+").unwrap());
static CHUNK_GAPS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n\s*\n\s*\n+").unwrap());

/// Splits text into ~`size`-character pieces (~450 tokens) that overlap a little, breaking at whitespace. Characters
/// are counted as people see them (a Bengali letter with its vowel sign is one).
pub fn chunks(text: &str, size: usize, overlap: usize, limit: usize) -> Vec<String> {
    if size == 0 || limit == 0 {
        return Vec::new();
    }
    let clean = CHUNK_BLANKS.replace_all(text, " ");
    let clean = CHUNK_GAPS.replace_all(&clean, "\n\n");
    chunk_graphemes(clean.trim().graphemes(true), size, overlap, limit)
}

/// Keep one chunk and a lookahead grapheme instead of an index of the whole document.
fn chunk_graphemes<'a>(mut graphemes: impl Iterator<Item = &'a str>, size: usize, overlap: usize, limit: usize) -> Vec<String> {
    let mut window = Vec::new();
    let mut out = Vec::new();
    while out.len() < limit {
        window.extend(graphemes.by_ref().take(size.saturating_add(1).saturating_sub(window.len())));
        if window.is_empty() {
            break;
        }
        let is_space = |index: usize| window[index].chars().all(char::is_whitespace);
        let mut end = size.min(window.len());
        if end < window.len() {
            if let Some(space) = (0..end).rev().find(|&index| is_space(index)) {
                if space > size / 2 {
                    end = space;
                }
            }
        }
        let chunk = window[..end].concat();
        let chunk = chunk.trim();
        if !chunk.is_empty() {
            out.push(chunk.to_string());
        }
        if end >= window.len() {
            break;
        }
        let mut next = if end >= overlap { end - overlap } else { end };
        if next == 0 {
            next = end;
        }
        // Start the next chunk on a word boundary.
        while next < end && !is_space(next) {
            next += 1;
        }
        window.drain(..next);
    }
    out
}

/// How long text is, in characters as people see them.
pub fn length(text: &str) -> usize {
    text.graphemes(true).count()
}

/// Plain text from RTF: the body's text, without font tables, pictures, headers and other destinations.
pub fn rtf(bytes: &[u8]) -> String {
    static SKIPPED: LazyLock<HashSet<&str>> = LazyLock::new(|| {
        [
            "fonttbl", "colortbl", "stylesheet", "info", "pict", "header", "headerl", "headerr", "headerf", "footer",
            "footerl", "footerr", "footerf", "object", "themedata", "colorschememapping", "latentstyles", "datastore",
            "xmlnstbl", "listtable", "listoverridetable", "rsidtbl", "generator", "mmathPr", "fldinst", "filetbl",
            "revtbl", "pgdsctbl", "listtext", "pntext", "pntxta", "pntxtb", "bkmkstart", "bkmkend", "xe", "tc",
        ]
        .into_iter()
        .collect()
    });
    #[derive(Clone, Copy)]
    struct Group {
        skip: bool,
        unicode_skip: usize,
    }
    struct State {
        out: String,
        /// \'hh bytes, decoded together (a multi-byte code page character).
        pending: Vec<u8>,
        stack: Vec<Group>,
        /// Characters still to skip after a \u (its fallback).
        to_skip: usize,
    }
    impl State {
        fn skipping(&self) -> bool {
            self.stack.last().is_some_and(|group| group.skip)
        }
        fn flush(&mut self) {
            if self.pending.is_empty() {
                return;
            }
            // Windows-1252 for the bytes of the usual \ansicpg1252 (Latin-1 with its extra punctuation).
            let text: String = self.pending.iter().map(|byte| windows_1252(*byte)).collect();
            if !self.skipping() {
                self.out.push_str(&text);
            }
            self.pending.clear();
        }
        fn emit(&mut self, ch: char) {
            self.flush();
            if self.to_skip > 0 {
                self.to_skip -= 1;
                return;
            }
            if !self.skipping() {
                self.out.push(ch);
            }
        }
    }
    let mut state = State { out: String::new(), pending: Vec::new(), stack: vec![Group { skip: false, unicode_skip: 1 }], to_skip: 0 };
    let is_letter = |byte: u8| byte.is_ascii_alphabetic();
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        match byte {
            b'{' => {
                state.flush();
                let top = *state.stack.last().expect("a group");
                state.stack.push(top);
                // A destination nobody reads ("{\*\…}") or one of the known non-text ones.
                let mut j = i + 1;
                if j + 1 < bytes.len() && bytes[j] == b'\\' && bytes[j + 1] == b'*' {
                    state.stack.last_mut().expect("a group").skip = true;
                } else if j < bytes.len() && bytes[j] == b'\\' {
                    j += 1;
                    let start = j;
                    while j < bytes.len() && is_letter(bytes[j]) {
                        j += 1;
                    }
                    if SKIPPED.contains(String::from_utf8_lossy(&bytes[start..j]).as_ref()) {
                        state.stack.last_mut().expect("a group").skip = true;
                    }
                }
                i += 1;
            }
            b'}' => {
                state.flush();
                if state.stack.len() > 1 {
                    state.stack.pop();
                }
                i += 1;
            }
            b'\\' => {
                i += 1;
                if i >= bytes.len() {
                    break;
                }
                let next = bytes[i];
                if next == b'\'' && i + 2 < bytes.len() {
                    if let Ok(value) = u8::from_str_radix(&String::from_utf8_lossy(&bytes[i + 1..i + 3]), 16) {
                        if state.to_skip > 0 {
                            state.to_skip -= 1;
                        } else {
                            state.pending.push(value);
                        }
                        i += 3;
                        continue;
                    }
                }
                if is_letter(next) {
                    let start = i;
                    while i < bytes.len() && is_letter(bytes[i]) {
                        i += 1;
                    }
                    let word = String::from_utf8_lossy(&bytes[start..i]).into_owned();
                    let digits_start = i;
                    if i < bytes.len() && bytes[i] == b'-' {
                        i += 1;
                    }
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    let parameter: Option<i64> = String::from_utf8_lossy(&bytes[digits_start..i]).parse().ok();
                    if i < bytes.len() && bytes[i] == b' ' {
                        i += 1; // the delimiter
                    }
                    match word.as_str() {
                        "par" | "line" | "sect" | "page" | "row" => state.emit('\n'),
                        "tab" | "cell" => state.emit('\t'),
                        "emdash" => state.emit('\u{2014}'),
                        "endash" => state.emit('\u{2013}'),
                        "lquote" => state.emit('\u{2018}'),
                        "rquote" => state.emit('\u{2019}'),
                        "ldblquote" => state.emit('\u{201C}'),
                        "rdblquote" => state.emit('\u{201D}'),
                        "bullet" => state.emit('\u{2022}'),
                        "uc" => state.stack.last_mut().expect("a group").unicode_skip = parameter.unwrap_or(1).max(0) as usize,
                        "u" => {
                            if let Some(mut value) = parameter {
                                if value < 0 {
                                    value += 65536;
                                }
                                state.emit(char::from_u32(value as u32).unwrap_or('\u{FFFD}'));
                                state.to_skip = state.stack.last().expect("a group").unicode_skip;
                            }
                        }
                        other => {
                            if SKIPPED.contains(other) {
                                state.stack.last_mut().expect("a group").skip = true;
                            }
                        }
                    }
                } else {
                    // \\ \{ \} are the characters themselves; \~ is a no-break space, \- an optional hyphen.
                    match next {
                        b'~' => state.emit('\u{00A0}'),
                        b'-' | b'*' => {}
                        b'\n' | b'\r' => state.emit('\n'),
                        other => state.emit(other as char),
                    }
                    i += 1;
                }
            }
            b'\n' | b'\r' => i += 1, // line breaks in the source mean nothing
            _ => {
                if state.to_skip > 0 {
                    state.to_skip -= 1;
                } else if byte < 0x80 {
                    state.emit(byte as char);
                } else {
                    state.pending.push(byte);
                }
                i += 1;
            }
        }
    }
    state.flush();
    state.out.trim().to_string()
}

fn windows_1252(byte: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž', '\u{8F}', '\u{90}', '‘',
        '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}', 'ž', 'Ÿ',
    ];
    match byte {
        0x80..=0x9F => HIGH[(byte - 0x80) as usize],
        _ => byte as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "manual performance comparison; run with --ignored --nocapture"]
    fn bounded_chunk_timings() {
        let text = "word ".repeat(200_000);
        let run = |f: fn(&str, usize, usize, usize) -> Vec<String>, limit| {
            let start = std::time::Instant::now();
            for _ in 0..10 {
                std::hint::black_box(f(std::hint::black_box(&text), 1800, 200, limit));
            }
            start.elapsed()
        };
        let previous = run(reference_chunks, 8);
        let bounded = run(chunks, 8);
        eprintln!("one million ASCII graphemes, 8 chunks, 10 iterations: previous={previous:?}, bounded={bounded:?}");
        let previous = run(reference_chunks, 1000);
        let bounded = run(chunks, 1000);
        eprintln!("one million ASCII graphemes, all chunks, 10 iterations: previous={previous:?}, bounded={bounded:?}");
    }

    #[test]
    fn sliding_chunks_match_the_previous_algorithm() {
        for text in ["", "one", "one two three four five six seven", "  a   b\n\n\n c  ",
                     "বাংলা ভাষা লেখা একটি পরীক্ষা", "cafe\u{301} 😀 family 👨‍👩‍👧‍👦 hello", "verylongwordwithoutanyspaces"] {
            for size in [1, 2, 5, 12, 30] {
                for overlap in [0, 1, 3, 20] {
                    for limit in [0, 1, 2, 6] {
                        assert_eq!(chunks(text, size, overlap, limit), reference_chunks(text, size, overlap, limit),
                                   "text={text:?}, size={size}, overlap={overlap}, limit={limit}");
                    }
                }
            }
        }
    }

    fn reference_chunks(text: &str, size: usize, overlap: usize, limit: usize) -> Vec<String> {
        let clean = CHUNK_BLANKS.replace_all(text, " ");
        let clean = CHUNK_GAPS.replace_all(&clean, "\n\n");
        let clean = clean.trim();
        let graphemes: Vec<&str> = clean.graphemes(true).collect();
        let is_space = |index: usize| graphemes[index].chars().all(char::is_whitespace);
        let mut out = Vec::new();
        let mut start = 0;
        while start < graphemes.len() && out.len() < limit {
            let mut end = (start + size).min(graphemes.len());
            if end < graphemes.len() {
                if let Some(space) = (start..end).rev().find(|&index| is_space(index)) {
                    if space - start > size / 2 {
                        end = space;
                    }
                }
            }
            let chunk = graphemes[start..end].concat();
            let chunk = chunk.trim();
            if !chunk.is_empty() {
                out.push(chunk.to_string());
            }
            if end >= graphemes.len() {
                break;
            }
            let mut next = if end >= start + overlap { end - overlap } else { end };
            if next <= start {
                next = end;
            }
            // Start the next chunk on a word boundary.
            while next < end && !is_space(next) {
                next += 1;
            }
            start = if next < end { next } else { end };
        }
        out
    }

    #[test]
    fn chunk_limit_bounds_grapheme_consumption() {
        let consumed = std::cell::Cell::new(0);
        let graphemes = std::iter::repeat_n("x", 1_000_000).inspect(|_| consumed.set(consumed.get() + 1));
        assert_eq!(chunk_graphemes(graphemes, 10, 2, 2), ["xxxxxxxxxx", "xxxxxxxxxx"]);
        assert_eq!(consumed.get(), 21);
        assert!(chunks("text", 0, 0, 10).is_empty());
        assert!(chunks("text", 10, 0, 0).is_empty());
    }

    #[test]
    fn utf16_decoding_keeps_byte_order_and_ignores_an_incomplete_unit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utf16.txt");
        for little in [true, false] {
            let mut bytes = if little { vec![0xFF, 0xFE] } else { vec![0xFE, 0xFF] };
            for unit in "Hello বাংলা 😀".encode_utf16() {
                bytes.extend(if little { unit.to_le_bytes() } else { unit.to_be_bytes() });
            }
            bytes.push(0xAA);
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(read_text(&path).unwrap(), "Hello বাংলা 😀");
        }
    }

    #[test]
    fn attributes_match_exact_names_and_decode_entities() {
        let tag = r#"<c other:r="wrong" r="B12" t="inlineStr" name="A &amp; B"/>"#;
        assert_eq!(attribute(tag, "r").as_deref(), Some("B12"));
        assert_eq!(attribute(tag, "name").as_deref(), Some("A & B"));
        assert_eq!(attribute(tag, "missing"), None);
        assert_eq!(attribute("<c\tr=\"C1\"/>", "r").as_deref(), Some("C1"));
    }

    #[test]
    fn sheet_headers_require_a_second_nonempty_row() {
        let row = vec![(0, "Name".to_string()), (2, "Count".to_string())];
        let mut out = String::new();
        assert!(!render_sheet(&mut out, "One", std::iter::once(row.clone()), 1000));
        assert_eq!(out, "Sheet: One\nName · Count\n\n");
        out.clear();
        let data = vec![(0, "Apples".to_string()), (2, "3".to_string()), (3, "extra".to_string())];
        assert!(!render_sheet(&mut out, "Two", [row, data].into_iter(), 1000));
        assert_eq!(out, "Sheet: Two\nColumns: Name · Count\nName: Apples · Count: 3 · extra\n\n");
        out.clear();
        assert!(!render_sheet(&mut out, "Empty", std::iter::empty(), 1000));
        assert!(out.is_empty());
    }

    #[test]
    fn sheet_cap_stops_consuming_rows() {
        let consumed = std::cell::Cell::new(0);
        let rows = (0..1_000_000).map(|_| {
            consumed.set(consumed.get() + 1);
            vec![(0, "1234567890".to_string())]
        });
        let mut out = String::new();
        assert!(render_sheet(&mut out, "Numbers", rows, 40));
        assert_eq!(consumed.get(), 3);
        assert_eq!(out, "Sheet: Numbers\n1234567890\n1234567890\n1234567890\n");
    }

    #[test]
    fn rtf_keeps_the_body_and_drops_tables() {
        let rtf_text = concat!(
            r"{\rtf1\ansi\ansicpg1252\deff0{\fonttbl{\f0\froman Times;}}{\colortbl;\red0\green0\blue0;}",
            "\n",
            r"{\*\generator Writer;}{\info{\title Lease}}",
            "\n",
            r"\pard\plain Section 7. Pets.\par",
            "\n",
            r"The tenant may keep one cat \endash  with consent.\tab Caf\'e9\par",
            "\n",
            r"{\*\bkmkstart pets}Unicode: \u1575?\u1604?\u1605? and \{braces\}.\par}",
        );
        assert_eq!(
            rtf(rtf_text.as_bytes()),
            "Section 7. Pets.\nThe tenant may keep one cat \u{2013} with consent.\tCafé\nUnicode: الم and {braces}."
        );
    }

    #[test]
    fn word_processor_xml_becomes_lines() {
        let docx = concat!(
            r#"<w:document><w:body><w:p><w:r><w:t>Pets &amp; rent</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>One</w:t><w:tab/><w:t>cat</w:t><w:br/><w:t>&#233;t&#xE9;</w:t></w:r></w:p><w:p/></w:body></w:document>"#
        );
        assert_eq!(xml_text(docx, "w:p", "w:tab", "w:br", None), "Pets & rent\nOne\tcat\nété");
        let odt = r#"<office:text><text:h>Lease</text:h><text:p>A<text:s/>b<text:line-break/>c</text:p></office:text>"#;
        assert_eq!(xml_text(odt, "text:p|text:h", "text:tab", "text:line-break", Some("text:s")), "Lease\nA b\nc");
    }

    #[test]
    fn html_becomes_text() {
        let html = "<html><head><title>x</title></head><body><script>var a</script><h1>Trip &amp; plans</h1>\
                    <p>Fly to Tokyo on Friday.</p><p>Caf&#233; &mdash; ramen</p></body></html>";
        assert_eq!(plain_text_from_html(html), "Trip & plans\nFly to Tokyo on Friday.\nCafé — ramen");
    }

    #[test]
    fn chunks_overlap_and_break_at_spaces() {
        let text: String = (0..400).map(|i| format!("word{i} ")).collect();
        let pieces = chunks(&text, 500, 100, 100);
        assert!(pieces.len() > 4);
        assert!(pieces.iter().all(|piece| length(piece) <= 500));
        assert!(pieces.iter().all(|piece| piece.starts_with("word")));
        // Each next chunk starts within the end of the last one.
        let last_word = pieces[0].split_whitespace().last().unwrap();
        assert!(pieces[1].contains(last_word));
    }
}
