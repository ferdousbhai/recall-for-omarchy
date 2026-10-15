//! The versioned, owner-local Recall service protocol. No client downloads models.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::embed::{Embedder, Embedding, WorkerError, WorkerInfo};
use serde_json::{Value, json};

pub const API_VERSION: u64 = 1;
pub const BATCH: usize = 32;

pub fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() })))
        .join("recall-for-omarchy.sock")
}

pub fn request(path: &Path, value: &Value, timeout: Duration) -> Result<Value, WorkerError> {
    let fail = |e: std::io::Error| WorkerError::Failed(e.to_string());
    let mut stream = UnixStream::connect(path).map_err(fail)?;
    stream.set_read_timeout(Some(timeout)).map_err(fail)?;
    stream.set_write_timeout(Some(timeout)).map_err(fail)?;
    serde_json::to_writer(&mut stream, value).map_err(|e| WorkerError::Failed(e.to_string()))?;
    stream.write_all(b"\n").map_err(fail)?;
    let mut bytes = Vec::new();
    use std::io::Read;
    BufReader::new(stream)
        .take(10_485_761)
        .read_until(b'\n', &mut bytes)
        .map_err(fail)?;
    if bytes.len() > 10_485_760 || bytes.last() != Some(&b'\n') {
        return Err(WorkerError::Failed("Invalid Recall response".into()));
    }
    let reply: Value =
        serde_json::from_slice(&bytes).map_err(|e| WorkerError::Failed(e.to_string()))?;
    if reply["api"] != API_VERSION {
        return Err(WorkerError::Failed(
            "Recall service API is unavailable; update Recall".into(),
        ));
    }
    if let Some(error) = reply["error"].as_str() {
        return Err(WorkerError::Failed(error.into()));
    }
    Ok(reply)
}

pub struct RemoteEmbedder {
    socket: PathBuf,
    info: WorkerInfo,
}
impl RemoteEmbedder {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            info: crate::llama::model_info(),
        }
    }
    fn send(
        &self,
        kind: &str,
        inputs: &[String],
        budget: u32,
    ) -> Result<Vec<Embedding>, WorkerError> {
        let mut output = Vec::new();
        for batch in inputs.chunks(if kind == "text" { BATCH } else { 8 }) {
            let reply = request(
                &self.socket,
                &json!({"api":API_VERSION,"op":"embed","kind":kind,"inputs":batch,"budget":budget}),
                Duration::from_secs(115),
            )?;
            if reply["source"] != self.info.vector_source() {
                return Err(WorkerError::Failed(
                    "Recall model version changed; restart the engine".into(),
                ));
            }
            let embeddings: Vec<Embedding> = serde_json::from_value(reply["embeddings"].clone())
                .map_err(|e| WorkerError::Failed(e.to_string()))?;
            if embeddings.len() != batch.len() {
                return Err(WorkerError::Failed("Invalid embedding batch".into()));
            }
            output.extend(embeddings);
        }
        Ok(output)
    }
}
impl Embedder for RemoteEmbedder {
    fn info(&self) -> &WorkerInfo {
        &self.info
    }
    fn embed_texts(&mut self, inputs: &[String]) -> Result<Vec<Vec<f32>>, WorkerError> {
        self.send("text", inputs, 280)?
            .into_iter()
            .map(|e| {
                e.vector
                    .ok_or_else(|| WorkerError::Failed(e.error.unwrap_or_default()))
            })
            .collect()
    }
    fn embed_images(
        &mut self,
        paths: &[String],
        budget: u32,
    ) -> Result<Vec<Embedding>, WorkerError> {
        self.send("image", paths, budget)
    }
    fn embed_audio(&mut self, paths: &[String]) -> Result<Vec<Embedding>, WorkerError> {
        self.send("audio", paths, 280)
    }
    // This client owns no model; another app may still be using the shared worker.
    fn close(&mut self) {}
}
