//! One persistent inference owner for Recall and every optional desktop client.
use super::*;
use recall::embed::{Embedder, EmbeddingWorker, WorkerConfig};
use recall::model::prompts;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::thread;

type Message = (Value, Value, mpsc::Sender<Result<Value>>);
pub struct Service {
    tx: mpsc::SyncSender<Message>,
}
impl Service {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::sync_channel::<Message>(32);
        thread::spawn(move || {
            let mut owner = Owner {
                worker: None,
                models: String::new(),
                media: false,
                cache: None,
                queries: HashMap::new(),
            };
            let mut idle = 120;
            loop {
                match rx.recv_timeout(Duration::from_secs(idle)) {
                    Ok((request, config, reply)) => {
                        idle = config["encoder_idle_seconds"].as_u64().unwrap_or(120);
                        let result = owner.handle(&request, &config);
                        if result.is_err() {
                            owner.worker = None;
                        }
                        let _ = reply.send(result);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => owner.worker = None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Self { tx }
    }
    pub fn request(&self, request: &Value, config: Value) -> Result<Value> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .try_send((request.clone(), config, tx))
            .context("Shared inference queue is busy")?;
        rx.recv_timeout(Duration::from_secs(115))
            .context("Shared inference timed out")?
    }
}

struct Owner {
    worker: Option<EmbeddingWorker>,
    models: String,
    media: bool,
    cache: Option<(PathBuf, Connection)>,
    queries: HashMap<(String, String), Vec<f32>>,
}
impl Owner {
    fn worker(&mut self, config: &Value, media: bool) -> Result<&mut EmbeddingWorker> {
        let models = config["models_directory"]
            .as_str()
            .context("Missing model directory")?;
        let text = Path::new(models).join(model_files::TEXT.name);
        if !text.is_file() || (media && !Path::new(models).join(model_files::MEDIA.name).is_file())
        {
            bail!("Recall models are not ready; keyword search remains available");
        }
        if self.models != models || (media && !self.media) {
            self.worker = None;
        }
        if self.worker.is_none() {
            let mut worker = WorkerConfig::helper(engine()?);
            worker.environment = vec![
                ("RECALL_MODELS".into(), models.into()),
                ("RECALL_SHARED_INFERENCE".into(), "1".into()),
            ];
            worker.log = Some(
                Path::new(config["index_directory"].as_str().unwrap()).join("shared-inference.log"),
            );
            self.worker = Some(EmbeddingWorker::start(&worker, !media)?);
            self.models = models.into();
            self.media = media;
        }
        Ok(self.worker.as_mut().unwrap())
    }
    fn database(&mut self, config: &Value) -> Result<&Connection> {
        let path =
            Path::new(config["index_directory"].as_str().unwrap()).join("collections.sqlite");
        if self.cache.as_ref().is_none_or(|(old, _)| old != &path) {
            let db = Connection::open(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            db.execute_batch("CREATE TABLE IF NOT EXISTS vectors (collection TEXT NOT NULL, id TEXT NOT NULL, source TEXT NOT NULL, digest TEXT NOT NULL, vector BLOB NOT NULL, PRIMARY KEY(collection,id));")?;
            self.cache = Some((path, db));
        }
        Ok(&self.cache.as_ref().unwrap().1)
    }
    fn handle(&mut self, request: &Value, config: &Value) -> Result<Value> {
        if request["api"] != recall::shared::API_VERSION {
            bail!("Unsupported Recall API version");
        }
        match request["op"].as_str() {
            Some("embed") => {
                let inputs: Vec<String> = serde_json::from_value(request["inputs"].clone())?;
                let kind = request["kind"].as_str().context("Missing embedding kind")?;
                if !["text", "image", "audio"].contains(&kind)
                    || inputs.is_empty()
                    || inputs.len() > if kind == "text" { 32 } else { 8 }
                    || inputs.iter().any(|s| s.len() > 65536)
                {
                    bail!("Invalid embedding batch");
                }
                let worker = self.worker(config, kind != "text")?;
                let embeddings = match kind {
                    "text" => worker
                        .embed_texts(&inputs)?
                        .into_iter()
                        .map(|v| recall::embed::Embedding {
                            vector: Some(v),
                            error: None,
                        })
                        .collect(),
                    "image" => worker.embed_images(
                        &inputs,
                        request["budget"].as_u64().unwrap_or(280).clamp(70, 1120) as u32,
                    )?,
                    _ => worker.embed_audio(&inputs)?,
                };
                Ok(json!({"api":1,"source":worker.info().vector_source(),"embeddings":embeddings}))
            }
            Some("records.search") => self.rank(request, config),
            Some("records.forget") => {
                let collection = collection(request)?;
                let db = self.database(config)?;
                if let Some(ids) = request["ids"].as_array() {
                    if ids.len() > 256 {
                        bail!("Too many record IDs");
                    }
                    for id in ids {
                        db.execute(
                            "DELETE FROM vectors WHERE collection=?1 AND id=?2",
                            params![collection, id.as_str().context("Invalid record ID")?],
                        )?;
                    }
                } else {
                    db.execute("DELETE FROM vectors WHERE collection=?1", [collection])?;
                }
                Ok(json!({"api":1,"ok":true}))
            }
            _ => bail!("Unknown shared-service operation"),
        }
    }
    fn rank(&mut self, request: &Value, config: &Value) -> Result<Value> {
        let collection = collection(request)?;
        let query = request["query"]
            .as_str()
            .filter(|q| !q.trim().is_empty() && q.chars().count() <= 4096)
            .context("Invalid query")?;
        let records = request["records"]
            .as_array()
            .filter(|v| v.len() <= 32)
            .context("Supply at most 32 records per batch")?;
        let limit = request["limit"].as_u64().unwrap_or(10).clamp(1, 32) as usize;
        let mut ids = HashSet::new();
        // Validate the whole batch before embedding or changing its cache.
        for record in records {
            let id = record["id"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 4096)
                .context("Invalid record ID")?;
            if !ids.insert(id) {
                bail!("Duplicate record ID");
            }
            if record["text"].as_str().is_some_and(|s| s.len() > 65536)
                || record["title"].as_str().is_some_and(|s| s.len() > 4096)
            {
                bail!("Record is too large");
            }
            if let Some(kind) = record["kind"].as_str() {
                if !["text", "image", "audio"].contains(&kind) {
                    bail!("Invalid record kind");
                }
                if kind != "text"
                    && !record["path"]
                        .as_str()
                        .is_some_and(|p| Path::new(p).is_absolute())
                {
                    bail!("Media paths must be absolute");
                }
            }
        }
        if records.is_empty() {
            return Ok(json!({"api":1,"meaning":true,"hits":[]}));
        }
        let source = recall::llama::model_info().vector_source();
        let key = (source.clone(), query.to_owned());
        let query_vector = if let Some(vector) = self.queries.get(&key) {
            vector.clone()
        } else {
            let vector = self
                .worker(config, false)?
                .embed_texts(&[prompts::query(query)])?
                .remove(0);
            if self.queries.len() >= 64 {
                self.queries.clear();
            }
            self.queries.insert(key, vector.clone());
            vector
        };
        let mut hits = Vec::new();
        for record in records {
            let id = record["id"].as_str().unwrap();
            let kind = record["kind"].as_str().unwrap_or("text");
            let mut digest = Sha256::new();
            digest.update(serde_json::to_vec(record)?);
            if kind != "text" {
                let metadata = fs::metadata(record["path"].as_str().unwrap())?;
                digest.update(metadata.len().to_le_bytes());
                digest.update(
                    metadata
                        .modified()?
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_nanos()
                        .to_le_bytes(),
                );
            }
            let digest = format!("{:x}", digest.finalize());
            let cached: Option<Vec<u8>> = self.database(config)?.query_row(
                "SELECT vector FROM vectors WHERE collection=?1 AND id=?2 AND source=?3 AND digest=?4",
                params![collection, id, source, digest], |r| r.get(0)).optional()?;
            let vector = match cached {
                Some(bytes) => bytes
                    .as_chunks::<4>().0.iter()
                    .map(|b| f32::from_le_bytes(*b))
                    .collect::<Vec<_>>(),
                None => {
                    let worker = self.worker(config, kind != "text")?;
                    let vector = if kind == "text" {
                        worker
                            .embed_texts(&[prompts::document(
                                record["title"].as_str(),
                                record["text"].as_str().unwrap_or(""),
                            )])?
                            .remove(0)
                    } else {
                        let paths = [record["path"].as_str().unwrap().to_owned()];
                        let embedding = if kind == "image" {
                            worker.embed_images(&paths, 280)?
                        } else {
                            worker.embed_audio(&paths)?
                        }
                        .remove(0);
                        embedding.vector.context(
                            embedding
                                .error
                                .unwrap_or_else(|| "Media could not be embedded".into()),
                        )?
                    };
                    let bytes: Vec<u8> = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
                    self.database(config)?.execute(
                        "INSERT OR REPLACE INTO vectors VALUES (?1,?2,?3,?4,?5)",
                        params![collection, id, source, digest, bytes],
                    )?;
                    vector
                }
            };
            if vector.len() != query_vector.len() {
                bail!("Invalid cached vector dimension");
            }
            let semantic: f32 = query_vector.iter().zip(&vector).map(|(a, b)| a * b).sum();
            let text = format!(
                "{} {}",
                record["title"].as_str().unwrap_or(""),
                record["text"].as_str().unwrap_or("")
            )
            .to_lowercase();
            let terms: Vec<_> = query.split_whitespace().collect();
            let lexical = terms
                .iter()
                .filter(|word| text.contains(&word.to_lowercase()))
                .count() as f32
                / terms.len().max(1) as f32;
            hits.push(json!({"id":id,"score":semantic + lexical * 0.2}));
        }
        hits.sort_by(|a, b| {
            b["score"]
                .as_f64()
                .partial_cmp(&a["score"].as_f64())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
        Ok(json!({"api":1,"meaning":true,"source":source,"hits":hits}))
    }
}
fn collection(request: &Value) -> Result<&str> {
    request["collection"]
        .as_str()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 128
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c))
        })
        .context("Invalid collection name")
}
