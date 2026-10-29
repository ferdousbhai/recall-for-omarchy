//! Office archive extraction uses no helper processes and bounds decompressed XML work.
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

fn archive(base: &Path, name: &str, method: CompressionMethod, members: &[(&str, &str)]) -> PathBuf {
    let path = base.join(name);
    let mut writer = ZipWriter::new(fs::File::create(&path).unwrap());
    for (name, text) in members {
        writer.start_file(*name, SimpleFileOptions::default().compression_method(method)).unwrap();
        writer.write_all(text.as_bytes()).unwrap();
    }
    writer.finish().unwrap();
    path
}

#[test]
fn stored_and_deflated_word_documents_read_without_external_tools() {
    let dir = tempfile::tempdir().unwrap();
    for (name, method) in [("stored.docx", CompressionMethod::Stored), ("compressed.docx", CompressionMethod::Deflated)] {
        let path = archive(dir.path(), name, method, &[
            ("word/document.xml", "<w:document><w:p>Hello &amp; 世界</w:p><w:p>next<w:tab/>line</w:p></w:document>"),
            ("a member with spaces ", "ignored"),
        ]);
        assert_eq!(recall::docs::text_of(&path).unwrap(), "Hello & 世界\nnext\tline");
        assert!(recall::docs::zip_members(&path).unwrap().iter().any(|(name, _)| name == "a member with spaces "));
        assert!(recall::docs::zipped_text_size(&path).unwrap() > 0);
    }
}

#[test]
fn missing_corrupt_and_oversized_xml_parts_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    let broken = dir.path().join("broken.docx");
    fs::write(&broken, b"not a zip").unwrap();
    assert!(recall::docs::text_of(&broken).is_err());
    let missing = archive(dir.path(), "missing.docx", CompressionMethod::Stored, &[("other.xml", "<x/>")]);
    assert!(recall::docs::text_of(&missing).is_err());
    let corrupt = archive(dir.path(), "corrupt.docx", CompressionMethod::Stored,
                          &[("word/document.xml", "<w:document><w:p>Hello</w:p></w:document>")]);
    let mut bytes = fs::read(&corrupt).unwrap();
    let at = bytes.windows(5).position(|w| w == b"Hello").unwrap();
    bytes[at] = b'J';
    fs::write(&corrupt, bytes).unwrap();
    assert!(recall::docs::text_of(&corrupt).is_err(), "a bad ZIP checksum must not be accepted");
    let huge = " ".repeat(8 * 1024 * 1024 + 1);
    let oversized = archive(dir.path(), "oversized.xlsx", CompressionMethod::Deflated, &[("xl/workbook.xml", &huge)]);
    assert!(recall::docs::text_of(&oversized).unwrap_err().to_string().contains("8 MiB"));
}

#[test]
fn shared_string_indices_keep_empty_entries_and_rich_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = archive(dir.path(), "book.xlsx", CompressionMethod::Deflated, &[
        ("xl/workbook.xml", "<workbook><sheets><sheet name=\"Data\" r:id=\"one\"/></sheets></workbook>"),
        ("xl/_rels/workbook.xml.rels", "<Relationships><Relationship Id=\"one\" Target=\"worksheets/sheet1.xml\"/></Relationships>"),
        ("xl/sharedStrings.xml", "<sst><si/><si><r><t>First</t></r><r><t> &amp; second</t></r><rPh><t>omit</t></rPh></si></sst>"),
        ("xl/worksheets/sheet1.xml", "<worksheet><row><c t=\"s\"><v>0</v></c><c t=\"s\"><v>1</v></c></row></worksheet>"),
    ]);
    assert_eq!(recall::docs::text_of(&path).unwrap(), "Sheet: Data\nFirst & second");
}

#[test]
fn office_extraction_never_starts_unzip() {
    if let Some(path) = std::env::var_os("RECALL_OFFICE_TEST_CHILD") {
        assert_eq!(recall::docs::text_of(Path::new(&path)).unwrap(), "Slide 1\nHello\nNotes: speaker notes");
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let deck = archive(dir.path(), "deck.pptx", CompressionMethod::Deflated, &[
        ("ppt/presentation.xml", "<p:presentation><p:sldId r:id=\"one\"/></p:presentation>"),
        ("ppt/_rels/presentation.xml.rels", "<Relationships><Relationship Id=\"one\" Target=\"slides/slide1.xml\"/></Relationships>"),
        ("ppt/slides/slide1.xml", "<p:sld><a:p>Hello</a:p></p:sld>"),
        ("ppt/slides/_rels/slide1.xml.rels", "<Relationships><Relationship Id=\"notes\" Type=\"http://x/notesSlide\" Target=\"../notesSlides/notesSlide1.xml\"/></Relationships>"),
        ("ppt/notesSlides/notesSlide1.xml", "<p:notes><a:p>speaker notes</a:p></p:notes>"),
    ]);
    let helper = dir.path().join("unzip");
    fs::write(&helper, "#!/bin/sh\nprintf called >> \"$RECALL_OFFICE_HELPER_LOG\"\nexit 1\n").unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
    let log = dir.path().join("helpers.log");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "office_extraction_never_starts_unzip"])
        .env("PATH", dir.path()).env("RECALL_OFFICE_TEST_CHILD", deck).env("RECALL_OFFICE_HELPER_LOG", &log)
        .output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(!log.exists(), "archive extraction launched an unzip process");
}
