//! System OCR coverage: use the same Tesseract engine supplied by Omarchy.
//! ocr.png is synthetic English text rendered in Liberation Sans on a plain white background.
mod common;
use std::path::Path;

#[test]
fn system_ocr_recovers_after_invalid_image_and_handles_path_spaces() {
    // OCR is optional for engine-only development. CI installs Tesseract and its English data, so there its absence
    // fails the test.
    if recall::tools::path("tesseract").is_none() {
        return common::skip("system_ocr_recovers_after_invalid_image_and_handles_path_spaces", "tesseract", common::in_ci());
    }
    let folder = tempfile::tempdir().unwrap();
    let invalid = folder.path().join("invalid.png");
    std::fs::write(&invalid, "not an image").unwrap();
    assert!(recall::ocr::text(&invalid).is_empty());
    let image = folder.path().join("image with spaces.png");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/reference/ocr.png"),
        &image,
    )
    .unwrap();
    let text = recall::ocr::text(&image);
    assert_eq!(text, "Recall for Omarchy\nPayment declined: retry with exponential backoff.\nRelease deadline: 14 October 2026.");
}

#[test]
fn cli_ocr_reports_languages_without_service() {
    if recall::tools::path("tesseract").is_none() {
        return;
    }
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_recall"))
        .arg("ocr")
        .env("XDG_RUNTIME_DIR", "/nonexistent-recall-runtime")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
    let reply: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(reply["engine"], "tesseract");
    assert!(reply["languages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|lang| lang == "eng"));
}
