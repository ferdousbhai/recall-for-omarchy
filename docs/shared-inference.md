# Optional shared inference API

Recall owns one lazy inference worker and keeps the pinned model files present. Text requests need only the text model. Media upgrades replace the text-only worker. Idle inference unloads according to `encoder_idle_seconds`.

## Model files

By default the weights are in the standard Hugging Face hub cache: `$HF_HUB_CACHE`, then `$HUGGINGFACE_HUB_CACHE`, `$HF_HOME/hub`, `$XDG_CACHE_HOME/huggingface/hub` and `~/.cache/huggingface/hub`, the order llama.cpp uses. The service reads these from its own environment, which is the systemd user manager's rather than your shell's; set them in `~/.config/environment.d/` if you move the cache. `recall config` and `recall capabilities` show the resulting `models_directory`.

The layout is the one `hf` writes, in `models--ggml-org--embeddinggemma-2-GGUF/`: `blobs/<sha256>`, `snapshots/<revision>/<file>` as relative symlinks, and `refs/main`. Downloads go to `blobs/<sha256>.incomplete` under `hf`'s lock in `.locks/`, and are SHA-256 verified before the blob and then the symlink appear. `refs/main` is written only if no tool has written it, because llama.cpp finds a cached repository through its refs; Recall itself never reads refs.

Recall uses only `snapshots/<revision>` at the revision pinned in `src/model_files.rs`, never `main`: upstream's `main` already differs from the pinned media file, and other weights would invalidate indexes. The service checksums what's in the snapshot when it starts, since any tool can write there. A wrong link is removed (its blob stays) and a wrong regular file is renamed to `<file>.invalid`; the right file is then downloaded. Recall deletes nothing else in the cache, and uninstalling leaves it alone. Deleting the repository with `hf` or by hand makes the service download it again while folders are selected.

`models_directory` isn't saved while it follows the default. These saved values also mean the default: the old `~/.local/share/recall/Models`, and a snapshot of this model at any revision in the current cache. A snapshot in another hub cache follows the pinned revision there. Any other folder is honoured as a plain folder of files: nothing else is searched when one is missing, a copy in the cache is hard-linked into it when possible, and otherwise it is downloaded there. `recall config set models_directory null` returns to the default.

An earlier default install moves over when the service starts: each file in the old folder is verified and linked (or, across filesystems, copied) into the cache, and the old names go only once all are in, so one complete folder is in use throughout. If the cache can't be written the old folder stays in use and the move is tried again at the next start. A file that fails its checksum is renamed to `<file>.invalid` and downloaded afresh.

## Other applications

Other applications must remain usable without Recall. They request meaning search explicitly, use a bounded background wait, and retain their native text search when the service is missing, incompatible, busy, warming up, or fails. They can load the cached files with their own runtime, read-only; to share Recall's worker and vectors, use this API.

## API

The owner-only Unix socket is `$XDG_RUNTIME_DIR/recall-for-omarchy.sock` (fallback `/run/user/UID/recall-for-omarchy.sock`). Requests and responses are newline-delimited JSON. External requests include `api: 1`; reject incompatible versions. `recall capabilities` reports supported operations, the pinned model source, `model_repository`, `model_revision` and `models_directory`.

`records.search` accepts `collection`, `query`, `records`, and `limit` (at most 32). Each record has a unique string `id`, `title`, and `text`; optional `kind: image|audio` requires an absolute local `path`. Each batch contains at most 32 records. Responses contain `api`, `meaning: true`, `source`, and scored `hits` with IDs. Errors contain `error` and must cause fallback. Clients merge chunk scores and validate every returned ID against the supplied records.

Supply a fresh, authorized record set for every query. Only those IDs can be returned; account/list/album/period filtering remains the application's responsibility. Collection names separate app caches and accept ASCII letters, digits, `-_.:` (128 characters maximum). The cache stores vectors, IDs, source fingerprints, and content digests, not record plaintext. Cached vectors are recomputed when content, media file metadata, or the model source changes.

`records.forget` removes a collection's cached vectors, optionally just supplied `ids`. `recall forget COLLECTION` clears a collection. `recall rank COLLECTION QUERY` accepts a JSON record array on stdin (32 MiB maximum), batches requests, and returns the best 100 hits. This is a convenience for explicit desktop/CLI calls, not an automatic indexer.

Internal Recall file/code indexing also delegates embeddings through this socket, so indexing and app search share the worker. Standalone engine commands remain supported. Hosted applications are outside this integration.
