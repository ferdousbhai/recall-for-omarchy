Recall for Omarchy v0.5.0 shares one local embedding service across desktop apps and reads Office documents without external tools.

- Other local apps can rank their own records through Recall's owner-only socket (`records.search`, `records.forget`, `recall rank`, `recall forget`), keeping their native text search as a fallback. Recall alone owns the model cache and the inference worker; its own indexing uses the same worker. See docs/shared-inference.md.
- DOCX, ODT, PPTX and XLSX are read in-process, so `zip` and `unzip` are no longer needed. ZIP checksums are verified, and extraction bounds metadata, XML scanning, worksheet rows and shared strings so a large compressed workbook cannot exhaust memory.
- Long documents are chunked without holding an index of the whole text.
- The app is shown as "Recall", with refined search and settings styling.

Existing indexes, models, and selected folders are preserved. OCR continues to use system Tesseract; CPU fallback and Vulkan acceleration remain available on both architectures.

Install: `curl -fsSL https://ferdousbhai.com/recall/install.sh | bash`

Based on pdstn12/Recall (MIT), inspired by ARahim3/DigUp.
