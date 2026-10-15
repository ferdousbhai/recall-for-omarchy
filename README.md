# Recall

Search your files by what’s in them. A native Omarchy desktop app **with a CLI and a bundled skill, so coding agents can search your files too**.

Describe a photo, find a document passage, jump to a recording interval, or search source code by intent. EmbeddingGemma 2 runs locally; file content, OCR text and embeddings stay on your computer.

## Install

```bash
curl -fsSL https://ferdousbhai.com/recall/install.sh | bash
```

The installer asks before installing packages (`bash -s -- --yes` or `RECALL_YES=1` skips the prompt). It adds a marked Super+Ctrl+Shift+F block to `~/.config/hypr/bindings.lua` after backing it up, and restores the file if Hyprland reports config errors; set `RECALL_NO_KEYBINDING=1` to skip it. If the GitHub API is rate limited, set `GITHUB_TOKEN` or pin `RECALL_VERSION=v0.4.2`.

Release assets from v0.4.3 on are signed with Sigstore (keyless, from this repo's release workflow). To verify a download:

```bash
cosign verify-blob recall-for-omarchy-x86_64.tar.gz \
  --bundle recall-for-omarchy-x86_64.tar.gz.sigstore.json \
  --certificate-identity-regexp '^https://github.com/ferdousbhai/recall-for-omarchy/.github/workflows/release.yml@refs/tags/v' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

Supports Omarchy on x86_64 and aarch64. The installer downloads a versioned, checksum-verified release, installs runtime dependencies through Omarchy, and configures the desktop integration and agent skill. Vulkan GPU acceleration is included on both architectures, with CPU fallback. Apple Silicon on Linux uses Asahi’s Honeykrisp driver; the installer adds `vulkan-asahi` on Apple machines. Embedding model files (about 865 MB: a 310 MB text model and a 555 MB image/audio encoder) download once from Hugging Face at a pinned revision, after you choose folders to index, and are SHA-256 verified. That download and the install itself are the only network access; search and indexing run offline. Set `RECALL_OFFLINE=1` to disable downloads (for example after copying the files into `~/.local/share/recall/Models` or your Hugging Face cache); `recall status` then reports `download: offline` until they are present. The model is [EmbeddingGemma](https://huggingface.co/ggml-org/embeddinggemma-2-GGUF), licensed under [Apache 2.0](https://www.apache.org/licenses/LICENSE-2.0) (see THIRD_PARTY_NOTICES.md). OCR uses Omarchy’s bundled Tesseract and English language data. Initial indexing takes time.

[Omarchy package PR #1066](https://github.com/omacom/omarchy-pkgs/pull/1066) awaits upstream build approval and publication. Once published in your configured channel:

```bash
omarchy pkg add recall-for-omarchy
recall setup
```

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
cargo test --release --locked
cargo clippy --all-targets -- -D warnings
omarchy/bundle.sh
```

## Credits and license

Based on [Recall by pdstn12](https://github.com/pdstn12/Recall), inspired by [DigUp by Abdur Rahim](https://github.com/ARahim3/DigUp). The Rust indexing/retrieval engine comes from Recall. This fork adds a native Rust CLI/coordinator and QML settings, alongside the native Omarchy panel, bar integration, bundled agent skill, packaging, and ARM fixes. MIT; see LICENSE and THIRD_PARTY_NOTICES.md. Upstream documentation: [pdstn12/Recall](https://github.com/pdstn12/Recall#readme).
