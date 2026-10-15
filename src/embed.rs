//! The four calls every embedding runtime answers: the model linked into this process (llama.cpp, `llama`), or a
//! worker process speaking JSON lines (`EmbeddingWorker`, `recall worker`). One thread at a time.

use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::vectors::DIMENSION;

/// What the embedding runtime reports about its model; part of the index fingerprint.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerInfo {
    pub model: String,
    pub revision: String,
    pub runtime: String,
    pub dtype: String,
    pub dim: usize,
    pub text_only: bool,
    pub load_seconds: Option<f64>,
}

impl WorkerInfo {
    /// Vectors from different models, revisions, runtimes or precisions must never share an index.
    pub fn vector_source(&self) -> String {
        let revision: String = self.revision.chars().take(7).collect();
        format!("{}@{} {} {}", self.model, revision, self.runtime, self.dtype)
    }
}

#[derive(Clone, Debug)]
pub enum WorkerError {
    NotFound(String),
    Failed(String),
    Died(String),
}

impl fmt::Display for WorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(why) => write!(f, "worker not found: {why}"),
            Self::Failed(why) => write!(f, "worker error: {why}"),
            Self::Died(why) => write!(f, "worker stopped: {why}"),
        }
    }
}

impl std::error::Error for WorkerError {}

/// One embedding (or why the file couldn't be embedded).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Embedding {
    pub vector: Option<Vec<f32>>,
    pub error: Option<String>,
}

pub trait Embedder: Send {
    fn info(&self) -> &WorkerInfo;
    fn embed_texts(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, WorkerError>;
    fn embed_images(&mut self, paths: &[String], budget: u32) -> Result<Vec<Embedding>, WorkerError>;
    fn embed_audio(&mut self, paths: &[String]) -> Result<Vec<Embedding>, WorkerError>;
    /// Frees the model. Safe to call twice.
    fn close(&mut self);
}

/// How to start a worker process that speaks the JSON-lines protocol.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub log: Option<PathBuf>,
    /// Runs the worker nicer than the app, for background indexing.
    pub background: bool,
}

impl WorkerConfig {
    /// The engine's own worker (`recall worker`): EmbeddingGemma 2 on llama.cpp.
    pub fn helper(executable: PathBuf) -> Self {
        Self { executable, arguments: vec!["worker".into()], environment: Vec::new(), log: None, background: false }
    }
}

#[derive(Deserialize)]
struct Reply {
    id: Option<i64>,
    ok: Option<bool>,
    ready: Option<bool>,
    error: Option<String>,
    dim: Option<usize>,
    vectors: Option<String>,
    errors: Option<Vec<Option<String>>>,
    info: Option<WorkerInfo>,
}

/// Talks to a worker process over stdin/stdout, one JSON line each way (the protocol is `recall worker`'s). The model
/// lives in that separate process so its memory goes back to the system when it exits.
pub struct EmbeddingWorker {
    info: WorkerInfo,
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    next_id: i64,
    log: Option<PathBuf>,
}

impl EmbeddingWorker {
    pub fn start(config: &WorkerConfig, text_only: bool) -> Result<Self, WorkerError> {
        let mut command = Command::new(&config.executable);
        command.args(&config.arguments);
        if text_only {
            command.arg("--text-only");
        }
        command.envs(config.environment.iter().map(|(k, v)| (k, v)));
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        match &config.log {
            Some(log) => {
                let file = std::fs::File::create(log)
                    .map_err(|error| WorkerError::NotFound(format!("can't write {}: {error}", log.display())))?;
                command.stderr(file);
            }
            None => {
                command.stderr(Stdio::null());
            }
        }
        let mut child = command
            .spawn()
            .map_err(|error| WorkerError::NotFound(format!("{}: {error}", config.executable.display())))?;
        if config.background {
            unsafe { libc::setpriority(libc::PRIO_PROCESS, child.id(), 10) };
        }
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().expect("piped"));
        let mut worker = Self {
            info: WorkerInfo {
                model: String::new(),
                revision: String::new(),
                runtime: String::new(),
                dtype: String::new(),
                dim: DIMENSION,
                text_only: false,
                load_seconds: None,
            },
            child,
            input,
            output,
            next_id: 1,
            log: config.log.clone(),
        };
        let ready = worker.read_reply()?;
        match (ready.ready, ready.info) {
            (Some(true), Some(info)) => worker.info = info,
            _ => {
                let why = ready.error.unwrap_or_else(|| format!("didn't start{}", worker.log_tail()));
                worker.close();
                return Err(WorkerError::Failed(why));
            }
        }
        Ok(worker)
    }

    fn send(&mut self, op: &str, inputs: &[String], budget: Option<u32>) -> Result<Vec<Embedding>, WorkerError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut request = serde_json::json!({"id": id, "op": op, "inputs": inputs});
        if let Some(budget) = budget {
            request["budget"] = budget.into();
        }
        let line = format!("{request}\n");
        let written = self.input.as_mut().map(|input| input.write_all(line.as_bytes()).and_then(|_| input.flush()));
        if !matches!(written, Some(Ok(()))) {
            return Err(WorkerError::Died(format!("can't write to the worker{}", self.log_tail())));
        }
        let reply = self.read_reply()?;
        if reply.id != Some(id) {
            return Err(WorkerError::Failed("reply out of order".into()));
        }
        let data = match (reply.ok, &reply.vectors) {
            (Some(true), Some(encoded)) => base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| WorkerError::Failed("bad vectors".into()))?,
            _ => return Err(WorkerError::Failed(reply.error.unwrap_or_else(|| "no vectors".into()))),
        };
        let dim = reply.dim.unwrap_or(DIMENSION);
        let floats: Vec<f32> =
            data.chunks_exact(4).map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])).collect();
        if floats.len() != dim * inputs.len() {
            return Err(WorkerError::Failed(format!("got {} floats", floats.len())));
        }
        let errors = reply.errors.unwrap_or_default();
        Ok((0..inputs.len())
            .map(|index| {
                let error = errors.get(index).cloned().flatten();
                let vector = error.is_none().then(|| floats[index * dim..(index + 1) * dim].to_vec());
                Embedding { vector, error }
            })
            .collect())
    }

    fn read_reply(&mut self) -> Result<Reply, WorkerError> {
        let deadline = Instant::now() + Duration::from_secs(110);
        let mut line = Vec::new();
        loop {
            if self.output.buffer().is_empty() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let mut fd = libc::pollfd { fd: self.output.get_ref().as_raw_fd(), events: libc::POLLIN, revents: 0 };
                let ready = unsafe { libc::poll(&mut fd, 1, remaining.as_millis().min(i32::MAX as u128) as i32) };
                if ready <= 0 { return Err(WorkerError::Failed("worker reply timed out".into())); }
            }
            let buffer = self.output.fill_buf().map_err(|error| WorkerError::Failed(error.to_string()))?;
            if buffer.is_empty() {
                return Err(WorkerError::Died(format!("exited{}", self.log_tail())));
            }
            let end = buffer.iter().position(|b| *b == b'\n').map(|i| i + 1);
            let count = end.unwrap_or(buffer.len());
            line.extend_from_slice(&buffer[..count]);
            self.output.consume(count);
            if line.len() > 10 * 1024 * 1024 { return Err(WorkerError::Failed("worker reply too large".into())); }
            if end.is_some() {
                if line.iter().all(u8::is_ascii_whitespace) { line.clear(); continue; }
                return serde_json::from_slice(&line).map_err(|error| WorkerError::Failed(format!("bad reply: {error}")));
            }
        }
    }

    fn log_tail(&self) -> String {
        let Some(log) = &self.log else { return String::new() };
        let Ok(text) = std::fs::read_to_string(log) else { return String::new() };
        let lines: Vec<&str> = text.lines().filter(|line| !line.is_empty()).collect();
        let tail = lines[lines.len().saturating_sub(6)..].join("\n  ");
        if tail.is_empty() { String::new() } else { format!("; worker log ({}):\n  {tail}", log.display()) }
    }
}

impl Embedder for EmbeddingWorker {
    fn info(&self) -> &WorkerInfo {
        &self.info
    }

    fn embed_texts(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, WorkerError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self.send("text", texts, None)?.into_iter().map(|e| e.vector.unwrap_or_default()).collect())
    }

    fn embed_images(&mut self, paths: &[String], budget: u32) -> Result<Vec<Embedding>, WorkerError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        self.send("image", paths, Some(budget))
    }

    fn embed_audio(&mut self, paths: &[String]) -> Result<Vec<Embedding>, WorkerError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        self.send("audio", paths, None)
    }

    fn close(&mut self) {
        if !matches!(self.child.try_wait(), Ok(None)) {
            return;
        }
        if let Some(mut input) = self.input.take() {
            let _ = input.write_all(b"{\"id\":0,\"op\":\"quit\"}\n");
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

impl Drop for EmbeddingWorker {
    fn drop(&mut self) {
        self.close();
    }
}
