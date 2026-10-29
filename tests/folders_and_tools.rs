//! Crawls over chosen and skipped folders, estimates by subfolder, keyword search over Bengali and Arabic text, and
//! poppler reading PDFs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use recall::crawler::{self, Candidate};
use recall::estimator::{self, Costs, Estimate};
use recall::model::{FileKind, IndexOptions, Modality, SegmentKind};
use recall::pdf::{self, PdfFile};
use recall::searcher::Searcher;
use recall::store::{Store, MAIN_FILE};
use recall::tools;

/// A temporary folder by its real path, as crawls report it.
fn folder() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().canonicalize().unwrap();
    (dir, real)
}

fn write(base: &Path, path: &str, text: &str) {
    let file = base.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, text).unwrap();
}

fn names(candidates: &[Candidate]) -> Vec<String> {
    let mut names: Vec<String> =
        candidates.iter().map(|c| Path::new(&c.path).file_name().unwrap().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[test]
fn nested_roots_are_crawled_once() {
    let (_dir, base) = folder();
    write(&base, "inner/trip.txt", "notes about the trip");
    let report = crawler::crawl(&[base.clone(), base.join("inner")], &IndexOptions::default());
    assert_eq!(names(&report.candidates), ["trip.txt"]);
}

#[test]
fn crawls_leave_skips_out_but_walk_chosen_folders_inside_them() {
    let (_dir, base) = folder();
    for (path, words) in [("D/top.txt", "top"), ("D/Media/film.txt", "film"), ("D/Media/Courses/week1.txt", "week one"),
                          ("D/Media/Courses/Old/old.txt", "old")] {
        write(&base, path, words);
    }
    let downloads = base.join("D");
    let courses = downloads.join("Media/Courses");
    let options = IndexOptions {
        excluded_folders: vec![text(&downloads.join("Media")), text(&courses.join("Old"))],
        ..IndexOptions::default()
    };
    assert_eq!(names(&crawler::crawl(&[downloads, courses], &options).candidates), ["top.txt", "week1.txt"]);
}

#[test]
fn estimates_come_by_subfolder() {
    let (_dir, base) = folder();
    for path in ["F/A/a.txt", "F/A/deeper/b.txt", "F/B/c.txt", "F/loose.txt"] {
        write(&base, path, &"words ".repeat(1000));
    }
    let root = text(&base.join("F"));
    let options = IndexOptions::default();
    let costs = Costs::default();
    let candidates = crawler::crawl(&[base.join("F")], &options).candidates;
    let parts = estimator::estimates_by_subfolder(&candidates, &root, &options, &costs);
    let keys: HashSet<String> = parts.keys().cloned().collect();
    assert_eq!(keys, HashSet::from([root.clone(), format!("{root}/A"), format!("{root}/B")]));
    assert_eq!(parts[&format!("{root}/A")].files.get(&FileKind::Doc), Some(&2));
    let whole = estimator::estimate(&candidates, &options, &costs);
    let sum = parts.values().cloned().fold(Estimate::default(), |sum, part| sum + part);
    assert!((sum.seconds - whole.seconds).abs() < 1e-9);
}

#[test]
fn subfolders_are_named_as_the_folder_was_given() {
    let (_dir, base) = folder();
    write(&base, "real/A/a.txt", "notes");
    // The folder as given goes through a symlink: subfolders are named after it, not the real path.
    std::os::unix::fs::symlink(base.join("real"), base.join("F")).unwrap();
    let given = base.join("F");
    let options = IndexOptions::default();
    let candidates = crawler::crawl(std::slice::from_ref(&given), &options).candidates;
    let parts = estimator::estimates_by_subfolder(&candidates, &text(&given), &options, &Costs::default());
    assert_eq!(parts.keys().cloned().collect::<Vec<_>>(), [format!("{}/A", text(&given))]);
    let options = IndexOptions { excluded_folders: vec![format!("{}/A", text(&given))], ..IndexOptions::default() };
    assert!(crawler::crawl(&[given], &options).candidates.is_empty());
}

fn candidate(path: &str, kind: FileKind) -> Candidate {
    Candidate { path: path.into(), kind, size: 1, mtime: 1.0, inode: 1, device: 1 }
}

#[test]
fn files_under_nested_folders_come_once() {
    let (_dir, base) = folder();
    let store = Store::open(&base.join("index"), MAIN_FILE).unwrap();
    store.insert(&candidate("/D/Media/Courses/a.mp4", FileKind::Video)).unwrap();
    assert_eq!(store.files_under(&["/D".into(), "/D/Media/Courses".into()]).unwrap().len(), 1);
}

#[test]
fn bengali_and_arabic_words_stay_whole() {
    let (_dir, base) = folder();
    let store = Store::open(&base, MAIN_FILE).unwrap();
    let id = store.insert(&candidate("/notes/trip.txt", FileKind::Doc)).unwrap();
    store
        .add_segment(id, SegmentKind::Chunk, Modality::Text, Some(1.0), None, None,
                     Some("সুন্দরবনে আমরা কুমিরও দেখেছি। قَالَ إِنَّ الْمَكْتَبَةَ تُغْلَقُ"), None)
        .unwrap();
    let mut searcher = Searcher::new(store, false).unwrap();
    let mut search = |query: &str| searcher.search(query, None, None, 20, 2.0, None, false).unwrap();
    let bengali = search("কুমির");
    assert_eq!(bengali.iter().map(|hit| hit.path.as_str()).collect::<Vec<_>>(), ["/notes/trip.txt"]);
    assert!(bengali[0].excerpt.as_deref().unwrap_or("").contains("«কুমিরও»")); // the whole word, never a piece of it
    assert_eq!(search("المكتبة").iter().map(|hit| hit.path.as_str()).collect::<Vec<_>>(), ["/notes/trip.txt"]);
    assert!(search("কলকাতা").is_empty());
}

/// Two pages, the second with a word on it, and a title.
fn two_page_pdf() -> Vec<u8> {
    let pages = ["BT /F1 12 Tf 72 720 Td (Morning at the river.) Tj ET", "BT /F1 12 Tf 72 720 Td (A quokka on the bridge.) Tj ET"];
    let mut objects: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [6 0 R 8 0 R] /Count 2 >>".into(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        "<< /Title (Field notes) >>".into(),
    ];
    for page in pages {
        objects.push(format!("<< /Length {} >>\nstream\n{page}\nendstream", page.len()));
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {} 0 R /Resources << /Font << /F1 3 0 R >> >> >>",
            objects.len()
        ));
    }
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf += &format!("{} 0 obj\n{object}\nendobj\n", index + 1);
    }
    let xref = pdf.len();
    pdf += &format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
    for offset in offsets {
        pdf += &format!("{offset:010} 00000 n \n");
    }
    pdf += &format!("trailer\n<< /Size {} /Root 1 0 R /Info 4 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1);
    pdf.into_bytes()
}

#[test]
fn poppler_reads_pages_and_title() {
    if tools::path("pdftotext").is_none() {
        return eprintln!("skipped: no poppler");
    }
    let (_dir, base) = folder();
    let path = base.join("two.pdf");
    std::fs::write(&path, two_page_pdf()).unwrap();
    let mut file = PdfFile::open(&path).unwrap();
    assert_eq!(file.page_count, 2);
    assert_eq!(file.title.as_deref(), Some("Field notes"));
    assert!(file.text(1).contains("quokka"));
    assert!(!file.text(0).contains("quokka"));
    assert_eq!(pdf::page_count(&path), Some(2));
    let page = file.render(0, 400, &base).expect("rendered");
    assert!(page.exists());
}

/// A real deflated Office archive; fixtures don't depend on a system zip tool.
fn zipped(base: &Path, name: &str, members: &[(&str, &str)]) -> PathBuf {
    use std::io::Write;
    let out = base.join(name);
    let mut archive = zip::ZipWriter::new(std::fs::File::create(&out).unwrap());
    for (member, text) in members {
        archive.start_file(*member, zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated)).unwrap();
        archive.write_all(text.as_bytes()).unwrap();
    }
    archive.finish().unwrap();
    out
}

#[test]
fn slides_come_in_the_decks_order_with_their_notes() {
    let (_dir, base) = folder();
    let slide = |text: &str| {
        format!(r#"<p:sld><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>{text}</a:t></a:r></a:p><a:p><a:r><a:t>second line</a:t></a:r><a:br/><a:r><a:t>&amp; more</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#)
    };
    let first = slide("Q3 budget review");
    let second = slide("Risks");
    let deck = zipped(&base, "deck.pptx", &[
        ("ppt/presentation.xml", r#"<p:presentation><p:sldIdLst><p:sldId id="256" r:id="rId3"/><p:sldId id="257" r:id="rId2"/></p:sldIdLst></p:presentation>"#),
        ("ppt/_rels/presentation.xml.rels", r#"<Relationships><Relationship Id="rId2" Type="http://x/slide" Target="slides/slide1.xml"/><Relationship Id="rId3" Type="http://x/slide" Target="/ppt/slides/slide2.xml"/></Relationships>"#),
        ("ppt/slides/slide1.xml", &first),
        ("ppt/slides/slide2.xml", &second),
        ("ppt/slides/_rels/slide1.xml.rels", r#"<Relationships><Relationship Id="rId1" Type="http://x/relationships/notesSlide" Target="../notesSlides/notesSlide1.xml"/></Relationships>"#),
        ("ppt/notesSlides/notesSlide1.xml", r#"<p:notes><a:p><a:r><a:t>Mention the Lyon lease</a:t></a:r></a:p><a:p><a:fld type="slidenum"><a:t>2</a:t></a:fld></a:p></p:notes>"#),
    ]);
    assert_eq!(
        recall::docs::text_of(&deck).unwrap(),
        "Slide 1\nRisks\nsecond line\n& more\n\nSlide 2\nQ3 budget review\nsecond line\n& more\nNotes: Mention the Lyon lease"
    );
}

#[test]
fn spreadsheet_rows_read_under_their_headers() {
    let (_dir, base) = folder();
    let book = zipped(&base, "book.xlsx", &[
        ("xl/workbook.xml", r#"<workbook><sheets><sheet name="Invoices" sheetId="1" r:id="rId1"/><sheet name="Secret" sheetId="2" state="hidden" r:id="rId2"/><sheet name="Readings" sheetId="3" r:id="rId3"/></sheets></workbook>"#),
        ("xl/_rels/workbook.xml.rels", r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Target="worksheets/sheet2.xml"/><Relationship Id="rId3" Target="worksheets/sheet3.xml"/></Relationships>"#),
        ("xl/sharedStrings.xml", r#"<sst><si><t>Invoice</t></si><si><t>Client</t></si><si><t>Due</t></si><si><r><t>INV-</t></r><r><t>2026-0187</t></r></si><si><t>Globex &amp; Co</t><rPh><t>x</t></rPh></si></sst>"#),
        ("xl/styles.xml", r#"<styleSheet><numFmts><numFmt numFmtId="164" formatCode="d&quot; days&quot; mmm yyyy"/></numFmts><cellXfs><xf numFmtId="0"/><xf numFmtId="164"/><xf numFmtId="14"/></cellXfs></styleSheet>"#),
        ("xl/worksheets/sheet1.xml", r#"<worksheet><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c><c r="D1" t="s"><v>2</v></c></row><row r="2"><c r="A2" t="s"><v>3</v></c><c r="B2" t="s"><v>4</v></c><c r="C2"><v>4200.5</v></c><c r="D2" s="1" t="n"><v>46310</v></c></row><row r="3"><c r="A3" t="inlineStr"><is><t>INV-2026-0188</t></is></c><c r="D3" s="2"><v>46310.5</v></c><c r="E3" t="e"><v>#REF!</v></c></row></sheetData></worksheet>"#),
        ("xl/worksheets/sheet2.xml", r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>hidden</t></is></c></row></sheetData></worksheet>"#),
        ("xl/worksheets/sheet3.xml", r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>2.5</v></c></row><row r="2"><c r="A2"><v>2</v></c><c r="B2" t="b"><v>1</v></c></row></sheetData></worksheet>"#),
    ]);
    assert_eq!(
        recall::docs::text_of(&book).unwrap(),
        "Sheet: Invoices\nColumns: Invoice · Client · Due\n\
         Invoice: INV-2026-0187 · Client: Globex & Co · 4200.5 · Due: 2026-10-15\n\
         Invoice: INV-2026-0188 · Due: 2026-10-15 12:00\n\n\
         Sheet: Readings\n1 · 2.5\n2 · TRUE"
    );
}

#[test]
fn zipped_documents_are_estimated_by_their_text() {
    let (_dir, base) = folder();
    let words = "<a:p><a:r><a:t>words</a:t></a:r></a:p>".repeat(100);
    let picture = "x".repeat(5_000_000);
    let deck = zipped(&base, "deck.pptx", &[("ppt/slides/slide1.xml", &words), ("ppt/media/image1.png", &picture)]);
    let size = recall::docs::zipped_text_size(&deck).unwrap();
    assert!(size > 0 && size < 2000, "{size}");
}
