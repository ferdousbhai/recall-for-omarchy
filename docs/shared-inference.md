# Optional shared inference API

Recall owns model downloads, the configured model directory, and one lazy inference worker. Existing verified weights are reused; a download lock serializes downloads, and verified Hugging Face cache files are linked when possible. Text requests need only the text model. Media upgrades replace the text-only worker. Idle inference unloads according to `encoder_idle_seconds`.

Other applications must remain usable without Recall. They request meaning search explicitly, use a bounded background wait, and retain their native text search when the service is missing, incompatible, busy, warming up, or fails. They must not download weights or spawn independent inference runtimes.

The owner-only Unix socket is `$XDG_RUNTIME_DIR/recall-for-omarchy.sock` (fallback `/run/user/UID/recall-for-omarchy.sock`). Requests and responses are newline-delimited JSON. External requests include `api: 1`; reject incompatible versions. `recall capabilities` reports supported operations and the pinned model source.

`records.search` accepts `collection`, `query`, `records`, and `limit` (at most 32). Each record has a unique string `id`, `title`, and `text`; optional `kind: image|audio` requires an absolute local `path`. Each batch contains at most 32 records. Responses contain `api`, `meaning: true`, `source`, and scored `hits` with IDs. Errors contain `error` and must cause fallback. Clients merge chunk scores and validate every returned ID against the supplied records.

Supply a fresh, authorized record set for every query. Only those IDs can be returned; account/list/album/period filtering remains the application's responsibility. Collection names separate app caches and accept ASCII letters, digits, `-_.:` (128 characters maximum). The cache stores vectors, IDs, source fingerprints, and content digests, not record plaintext. Cached vectors are recomputed when content, media file metadata, or the model source changes.

`records.forget` removes a collection's cached vectors, optionally just supplied `ids`. `recall forget COLLECTION` clears a collection. `recall rank COLLECTION QUERY` accepts a JSON record array on stdin (32 MiB maximum), batches requests, and returns the best 100 hits. This is a convenience for explicit desktop/CLI calls, not an automatic indexer.

Internal Recall file/code indexing also delegates embeddings through this socket, so indexing and app search share the worker. Standalone engine commands remain supported. Hosted applications are outside this integration.
