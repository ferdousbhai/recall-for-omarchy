# Recall

Search your files by what’s in them. A native Omarchy desktop app **with a CLI and a bundled skill, so coding agents can search your files too**.

Describe a photo, find a document passage, jump to a recording interval, or search source code by intent. EmbeddingGemma 2 runs locally; file content, OCR text and embeddings stay on your computer.

## Install

```bash
curl -fsSL https://ferdousbhai.com/recall/install.sh | sudo bash
```

Supports Omarchy (and other Arch Linux) on x86_64 and aarch64. The installer adds Recall's signed pacman repository (the package-signing key is checked against the fingerprint pinned in `install.sh`), installs the `recall-for-omarchy` package with its runtime dependencies, and then runs `recall setup` as you, in your desktop session: the background service, the search panel, the agent skill, and a marked Super+Ctrl+Shift+F block in `~/.config/hypr/bindings.lua` (backed up first, and restored if Hyprland reports config errors; `| sudo RECALL_NO_KEYBINDING=1 bash` skips it). Updates arrive with the rest of the system through `omarchy update`. Re-running the installer is safe.

Installed with an earlier version of this installer? Run the command above once: it replaces the copy in `~/.local/share/recall-for-omarchy` with the package and keeps your settings, index and models.

Vulkan GPU acceleration is included on both architectures, with CPU fallback. Apple Silicon on Linux uses Asahi’s Honeykrisp driver; the installer adds `vulkan-asahi` on Apple machines. A first setup selects `~/Documents` and `~/Pictures` (when they exist); change folders in Settings. Embedding model files (about 865 MB: a 310 MB text model and a 555 MB image/audio encoder) download once from Hugging Face at a pinned revision when there are folders to index, and are SHA-256 verified. That download and the install itself are the only network access; search and indexing run offline. Set `RECALL_OFFLINE=1` to disable downloads (for example after copying the files into `~/.local/share/recall/Models` or your Hugging Face cache); `recall status` then reports `download: offline` until they are present. The model is [EmbeddingGemma](https://huggingface.co/ggml-org/embeddinggemma-2-GGUF), licensed under [Apache 2.0](https://www.apache.org/licenses/LICENSE-2.0) (see THIRD_PARTY_NOTICES.md). OCR uses Omarchy’s bundled Tesseract and English language data. Initial indexing takes time.

The package installs for every user; `recall setup` is per user. Another user on the same machine runs `recall setup` once.

OCR works through the system `tesseract` executable, without Python or a separate environment. `recall ocr` reports installed recognition languages; additional Tesseract language packs are picked up when indexing workers restart. Images still support visual semantic search when OCR is unavailable.

## Desktop and agents

**Super+Ctrl+Shift+F** opens search. Arrow keys select, Enter opens at the matching page/line/media interval, and Ctrl+Enter reveals the file. Settings chooses folders; Pause stops background indexing.

```bash
recall search "payment error" --kind screenshot --limit 5
recall search "retry with exponential backoff" --code
recall text 123 --code
recall status
recall doctor
recall skill
recall skill install
```

The CLI and background coordinator are native Rust. The search panel and settings use QML. Python and GTK are not required at runtime. The CLI outputs JSON with meaningful exit codes. The embedded skill installs to the shared agents skill directory, with Claude Code linking when available. Searching never opens a window. The background service must be running. Only selected folders are searched; code repositories require explicit selection. Audio matches point to intervals, not word-level transcripts.

[Usage and development](omarchy/README.md) · [Website](https://ferdousbhai.com/recall)

## Build

```bash
export PATH="$HOME/.cargo/bin:$PATH"
scripts/build-llama.sh
cargo build --release --locked
bin/check                         # tests, clippy, scripts and the installer test
bin/build-packages "$(uname -m)"  # the native Arch package into dist/<arch>; no Docker
```

`bin/release <version>` publishes a release: write `packaging/release-notes.md` for it first. It bumps the version, runs `bin/check`, tags, builds both architectures in CI, signs the packages and repository locally, and publishes them with `install.sh`; CI then installs the published release in a fresh Arch root.

## Credits and license

Based on [Recall by pdstn12](https://github.com/pdstn12/Recall), inspired by [DigUp by Abdur Rahim](https://github.com/ARahim3/DigUp). The Rust indexing/retrieval engine comes from Recall. This fork adds a native Rust CLI/coordinator and QML settings, alongside the native Omarchy panel, bar integration, bundled agent skill, packaging, and ARM fixes. MIT; see LICENSE and THIRD_PARTY_NOTICES.md. Upstream documentation: [pdstn12/Recall](https://github.com/pdstn12/Recall#readme).

Other local apps can use the [optional shared inference API](docs/shared-inference.md) with native text-search fallbacks. Recall alone owns the model cache and inference worker.
