//! `recall serve`: search for the app, as a process: keeps the index open and the query encoder warm, and answers
//! searches as you type.
//!
//! Protocol: one JSON request per line on stdin, one JSON reply per line on stdout, with the request's "id".
//!   {"id": 1, "op": "configure", "roots": [...], "skipped": [...], "types": ["heic"], "code_roots": [...],
//!    "code_skipped": [...]}                          the folders searched (hits elsewhere are left out)
//!   {"id": 2, "op": "prewarm"}                       loads the vectors, and starts the encoder once no search waits
//!                                                    (the panel opened)
//!   {"id": 3, "op": "search", "query": "zebra in a video", "meaning": true, "prefix_last": true,
//!    "kinds": ["video"], "limit": 40}                "code: …" searches the code index
//!      → {"id": 3, "hits": [...], "strong": 5, "meaning": true, "code": false}
//!        or {"id": 3, "skipped": true} when a newer search was already waiting
//!   {"id": 4, "op": "text", "segment": 812, "code": false}   → {"id": 4, "text": "…"}   (a hit's passage)
//!   {"id": 5, "op": "roots", "code": false}         → {"id": 5, "roots": [{path, files, indexed, pending, …}]}
//!   {"id": 9, "op": "rank", "query": "…", "texts": ["…", …]}   → {"id": 9, "scores": [0.41, …]}  (a preview's
//!                                                    sentences against the query, to mark the one that matched)
//!   {"id": 6, "op": "reload"}                        the index changed (indexing): load the vectors again
//!   {"id": 7, "op": "release"}                       the panel closed for good: free the vectors and the encoder
//!   {"id": 8, "op": "quit"}                          (EOF too)
//! Errors come back as {"id": n, "error": "…"}.
//!
//! "meaning": false is a keyword search only (names, OCR, document text): instant, for the first results while the
//! encoder starts. The encoder (the text model alone, ~0.25 GB) exits after `--idle` seconds without a search.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use recall::embed::Embedder;
use recall::folders;
use recall::llama::LlamaModels;
use recall::model::{code_query, prompts, FileKind};
use recall::searcher::{self, Searcher};
use recall::store::{self, Store};
use recall::text;

use super::{emit, hit_row, make_embedder, Arguments};

#[derive(Default)]
struct Inbox {
    queue: VecDeque<Value>,
    closed: bool,
}

type Shared = Arc<(Mutex<Inbox>, Condvar)>;

struct Service {
    index_folder: PathBuf,
    idle_seconds: f64,
    searcher: Option<Searcher>,
    code_searcher: Option<Searcher>,
    encoder: Option<Box<dyn Embedder>>,
    encoder_failed: Option<String>,
    want_encoder: bool,
    last_use: Instant,
    /// Prompted query → vector.
    cache: HashMap<String, Vec<f32>>,
    cache_order: VecDeque<String>,
    roots: Vec<String>,
    skipped: Vec<String>,
    types: HashSet<String>,
    code_roots: Vec<String>,
    code_skipped: Vec<String>,
    inbox: Shared,
}

/// The app's search host.
pub fn serve(args: &Arguments) -> i32 {
    // Queries are a few words: the CPU encodes one in milliseconds and starts in under a second, where Vulkan takes
    // seconds to start (Iris Xe: 5.4 s against 0.7 s). The GPU stays free for indexing.
    if std::env::var_os("RECALL_DEVICE").is_none() {
        std::env::set_var("RECALL_DEVICE", "cpu");
    }
    let inbox: Shared = Arc::new((Mutex::new(Inbox::default()), Condvar::new()));
    let reader = inbox.clone();
    std::thread::spawn(move || read_requests(reader));
    let mut service = Service {
        index_folder: args.index_folder(),
        idle_seconds: args.number("--idle").unwrap_or(600.0),
        searcher: None,
        code_searcher: None,
        encoder: None,
        encoder_failed: None,
        want_encoder: false,
        last_use: Instant::now(),
        cache: HashMap::new(),
        cache_order: VecDeque::new(),
        roots: Vec::new(),
        skipped: Vec::new(),
        types: HashSet::new(),
        code_roots: Vec::new(),
        code_skipped: Vec::new(),
        inbox,
    };
    while let Some(request) = service.next() {
        let id = request.get("id").cloned().unwrap_or(json!(0));
        match service.handle(&request) {
            Ok(mut reply) => {
                reply["id"] = id;
                emit(reply);
                if request["op"] == "quit" {
                    break;
                }
            }
            Err(error) => emit(json!({"id": id, "error": error.to_string()})),
        }
    }
    if let Some(mut encoder) = service.encoder.take() {
        encoder.close();
    }
    0
}

fn read_requests(inbox: Shared) {
    let (lock, condition) = &*inbox;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str::<Value>(&line) else { continue };
        if !request.is_object() {
            continue;
        }
        lock.lock().unwrap().queue.push_back(request);
        condition.notify_all();
    }
    lock.lock().unwrap().closed = true;
    condition.notify_all();
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().map(|list| list.iter().filter_map(|item| item.as_str().map(String::from)).collect()).unwrap_or_default()
}

impl Service {
    /// The next request; while none comes, the encoder is stopped once it's been idle long enough.
    fn next(&mut self) -> Option<Value> {
        let inbox = self.inbox.clone();
        let (lock, condition) = &*inbox;
        loop {
            let mut state = lock.lock().unwrap();
            while state.queue.is_empty() && !state.closed {
                // The encoder asked for by "prewarm" starts once nothing's waiting, so searches by words never wait
                // for it.
                if self.want_encoder {
                    self.want_encoder = false;
                    drop(state);
                    let _ = self.query_encoder();
                    state = lock.lock().unwrap();
                    continue;
                }
                if self.encoder.is_some() && self.last_use.elapsed().as_secs_f64() > self.idle_seconds {
                    drop(state);
                    self.release_all();
                    state = lock.lock().unwrap();
                    continue;
                }
                let wait = Duration::from_secs(if self.encoder.is_some() { 5 } else { 3600 });
                state = condition.wait_timeout(state, wait).unwrap().0;
            }
            let request = state.queue.pop_front()?;
            // A search typed over by a newer one isn't worth running. (A search by words isn't overtaken by the search
            // with meaning for the same words: it answers at once, while that one may wait for the encoder.)
            let meaning = |request: &Value| request["meaning"].as_bool().unwrap_or(true);
            let overtaken = request["op"] == "search"
                && state.queue.iter().any(|newer| {
                    newer["op"] == "search" && (newer["query"] != request["query"] || meaning(newer) == meaning(&request))
                });
            if overtaken {
                emit(json!({"id": request.get("id").cloned().unwrap_or(json!(0)), "skipped": true}));
                continue;
            }
            return Some(request);
        }
    }

    fn handle(&mut self, request: &Value) -> Result<Value> {
        match request["op"].as_str() {
            Some("configure") => {
                self.roots = strings(&request["roots"]);
                self.skipped = strings(&request["skipped"]);
                self.types = strings(&request["types"]).into_iter().map(|kind| kind.to_lowercase()).collect();
                self.code_roots = strings(&request["code_roots"]);
                self.code_skipped = strings(&request["code_skipped"]);
                Ok(json!({"ok": true}))
            }
            Some("prewarm") => {
                self.last_use = Instant::now();
                if let Some(searcher) = self.main_searcher()? {
                    if !searcher.vectors_loaded {
                        searcher.reload()?;
                    }
                }
                self.want_encoder = self.encoder.is_none();
                Ok(json!({"ok": true}))
            }
            Some("search") => self.search(request),
            Some("text") => {
                let code = request["code"].as_bool().unwrap_or(false);
                let segment = request["segment"].as_i64().ok_or_else(|| anyhow!("which segment?"))?;
                let searcher = if code { self.code_searcher_if_any()? } else { self.main_searcher()? };
                let (text, path) = match searcher {
                    Some(searcher) => (searcher.text_of_segment(segment)?, searcher.path_of_segment(segment)?),
                    None => (None, None),
                };
                Ok(json!({"text": text, "path": path}))
            }
            Some("roots") => Ok(json!({"roots": self.root_summaries(request["code"].as_bool().unwrap_or(false))?})),
            Some("rank") => self.rank(request),
            Some("reload") => {
                // The vectors load again on the next search with meaning, not now: indexing can call this often.
                for searcher in [self.searcher.as_mut(), self.code_searcher.as_mut()].into_iter().flatten() {
                    searcher.invalidate()?;
                }
                Ok(json!({"ok": true}))
            }
            Some("release") => {
                self.release_all();
                Ok(json!({"ok": true}))
            }
            Some("quit") => Ok(json!({"ok": true})),
            op => Err(anyhow!("unknown op {}", op.unwrap_or("nil"))),
        }
    }

    // MARK: Search

    fn search(&mut self, request: &Value) -> Result<Value> {
        self.last_use = Instant::now();
        let mut query = request["query"].as_str().unwrap_or("").trim().to_string();
        let code_text = code_query::text(&query);
        let code = code_text.is_some();
        if let Some(code_text) = code_text {
            query = code_text;
        }
        let available = if code { self.code_searcher_if_any()?.is_some() } else { self.main_searcher()?.is_some() };
        if query.is_empty() || !available {
            return Ok(json!({"hits": [], "strong": 0, "meaning": false, "code": code}));
        }
        let mut vector = None;
        let wants_meaning = request["meaning"].as_bool().unwrap_or(true);
        let searcher = self.searcher_for(code);
        if wants_meaning && (searcher.vector_count() > 0 || !searcher.vectors_loaded) {
            let prompted = if code { prompts::code_query(&query) } else { prompts::query(&query) };
            vector = self.query_vector(&prompted, code).ok().flatten();
        }
        let kinds: Option<HashSet<FileKind>> = request["kinds"]
            .as_array()
            .map(|kinds| kinds.iter().filter_map(|kind| kind.as_str().and_then(FileKind::parse)).collect());
        let limit = request["limit"].as_u64().unwrap_or(40) as usize;
        let prefix_last = request["prefix_last"].as_bool().unwrap_or(false);
        let searcher = self.searcher_for(code);
        let hits = searcher.search(&query, vector.as_deref(), if code { None } else { kinds.as_ref() }, limit, 2.0,
                                   None, prefix_last)?;
        let (folders, skips) = if code { (&self.code_roots, &self.code_skipped) } else { (&self.roots, &self.skipped) };
        let hits: Vec<_> = hits
            .into_iter()
            .filter(|hit| {
                let extension = Path::new(&hit.path)
                    .extension()
                    .map(|extension| extension.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                (folders.is_empty() || folders::is_searched(&hit.path, folders, skips))
                    && (code || !self.types.contains(&extension))
                    && Path::new(&hit.path).exists()
            })
            .collect();
        Ok(json!({
            "hits": hits.iter().map(|hit| hit_row(hit, true)).collect::<Vec<_>>(),
            "strong": searcher::strong_count(&hits, 0.5),
            "meaning": vector.is_some(),
            "code": code,
            "encoder_error": self.encoder_failed,
        }))
    }

    /// How well each of `texts` (a page's sentences, a screenshot's lines) matches the query: the preview marks the
    /// best one, where a result matched by meaning without the query's words.
    fn rank(&mut self, request: &Value) -> Result<Value> {
        self.last_use = Instant::now();
        let mut query = request["query"].as_str().unwrap_or("").trim().to_string();
        let code = code_query::text(&query);
        if let Some(code) = &code {
            query = code.clone();
        }
        let texts: Vec<String> =
            strings(&request["texts"]).iter().take(400).map(|text| text::prefix(text, 1000).to_string()).collect();
        if query.is_empty() || texts.is_empty() {
            return Ok(json!({"scores": []}));
        }
        let Some(encoder) = self.query_encoder()? else { return Ok(json!({"scores": []})) };
        let prompted = if code.is_none() { prompts::query(&query) } else { prompts::code_query(&query) };
        let inputs: Vec<String> =
            std::iter::once(prompted).chain(texts.iter().map(|text| prompts::document(None, text))).collect();
        let vectors = encoder.embed_texts(&inputs)?;
        let Some(query_vector) = vectors.first() else { return Ok(json!({"scores": []})) };
        let scores: Vec<f64> = vectors[1..]
            .iter()
            .map(|vector| query_vector.iter().zip(vector).map(|(a, b)| f64::from(a * b)).sum())
            .collect();
        Ok(json!({"scores": scores}))
    }

    /// The query's vector: from the cache, else the encoder. None when the encoder can't run or makes vectors from a
    /// different model than the index's (then the search is by words only).
    fn query_vector(&mut self, prompted: &str, code: bool) -> Result<Option<Vec<f32>>> {
        if let Some(cached) = self.cache.get(prompted) {
            return Ok(Some(cached.clone()));
        }
        let indexed = self.searcher_for(code).vector_source.clone();
        let Some(encoder) = self.query_encoder()? else { return Ok(None) };
        if indexed.is_some_and(|indexed| indexed != encoder.info().vector_source()) {
            return Ok(None);
        }
        let Some(vector) = encoder.embed_texts(&[prompted.to_string()])?.into_iter().next() else { return Ok(None) };
        self.cache.insert(prompted.to_string(), vector.clone());
        self.cache_order.push_back(prompted.to_string());
        if self.cache_order.len() > 256 {
            if let Some(oldest) = self.cache_order.pop_front() {
                self.cache.remove(&oldest);
            }
        }
        Ok(Some(vector))
    }

    fn query_encoder(&mut self) -> Result<Option<&mut Box<dyn Embedder>>> {
        if self.encoder.is_none() {
            if std::env::var_os("RECALL_SHARED_SOCKET").is_none() && LlamaModels::locate().is_err() {
                self.encoder_failed = Some("the model isn't downloaded yet".into());
                return Ok(None);
            }
            match make_embedder(true, &self.index_folder.join("worker-query.log")) {
                Ok(encoder) => {
                    self.encoder = Some(encoder);
                    self.encoder_failed = None;
                }
                Err(error) => {
                    self.encoder_failed = Some(error.to_string());
                    return Err(error.into());
                }
            }
        }
        Ok(self.encoder.as_mut())
    }

    fn release_all(&mut self) {
        if let Some(mut encoder) = self.encoder.take() {
            encoder.close();
        }
        for searcher in [self.searcher.as_mut(), self.code_searcher.as_mut()].into_iter().flatten() {
            searcher.release_vectors(); // back to the system at once
        }
    }

    fn searcher_for(&mut self, code: bool) -> &mut Searcher {
        if code { self.code_searcher.as_mut() } else { self.searcher.as_mut() }.expect("opened")
    }

    fn main_searcher(&mut self) -> Result<Option<&mut Searcher>> {
        if self.searcher.is_none() {
            std::fs::create_dir_all(&self.index_folder)?;
            let store = Store::open(&self.index_folder, store::MAIN_FILE)?;
            self.searcher = Some(Searcher::new(store, false)?);
        }
        Ok(self.searcher.as_mut())
    }

    /// The code index, when it's there (code search is on).
    fn code_searcher_if_any(&mut self) -> Result<Option<&mut Searcher>> {
        if self.code_searcher.is_none() {
            if !self.index_folder.join(store::CODE_FILE).exists() {
                return Ok(None);
            }
            let store = Store::open(&self.index_folder, store::CODE_FILE)?;
            self.code_searcher = Some(Searcher::new(store, false)?);
        }
        Ok(self.code_searcher.as_mut())
    }

    // MARK: Folders

    /// What's indexed under each chosen folder.
    fn root_summaries(&mut self, code: bool) -> Result<Vec<Value>> {
        let roots = if code { self.code_roots.clone() } else { self.roots.clone() };
        let searcher = if code { self.code_searcher_if_any()? } else { self.main_searcher()? };
        let Some(searcher) = searcher else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for root in roots {
            let mut counts: BTreeMap<&str, usize> =
                ["files", "indexed", "pending", "failed", "skipped", "unfinished"].into_iter().map(|key| (key, 0)).collect();
            let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
            for file in searcher.store.files_under(std::slice::from_ref(&root))? {
                *counts.get_mut("files").unwrap() += 1;
                *kinds.entry(file.kind.as_str().to_string()).or_insert(0) += 1;
                let keys: &[&str] = match file.state.as_str() {
                    "done" => &["indexed"],
                    "partial" => &["indexed", "unfinished"],
                    "pending" => &["pending"],
                    "failed" => &["failed"],
                    _ => &["skipped"],
                };
                for key in keys {
                    *counts.get_mut(key).unwrap() += 1;
                }
            }
            let mut summary = json!({"path": root, "missing": !Path::new(&root).is_dir(), "kinds": kinds});
            for (key, value) in counts {
                summary[key] = value.into();
            }
            out.push(summary);
        }
        Ok(out)
    }
}
