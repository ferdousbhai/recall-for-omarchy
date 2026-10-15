//! What the commands share: arguments, the embedder, JSON lines out, formatting.

pub mod index_service;
pub mod serve;
pub mod worker;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde_json::Value;

use recall::embed::{Embedder, WorkerError};
use recall::llama::{self, LlamaEmbedder, LlamaModels};
use recall::model::{FileKind, IndexOptions, SegmentKind};
use recall::paths;
use recall::searcher::SearchHit;
use recall::store::{self, Store};

#[derive(Default, Debug)]
pub struct Arguments {
    pub positional: Vec<String>,
    pub values: HashMap<String, String>,
    pub lists: HashMap<String, Vec<String>>,
    pub flags: HashSet<String>,
}

const VALUED: [&str; 11] = ["--index", "--idle", "--limit", "--kind", "-n", "--zgate", "--word-weight", "--new-limit",
                            "--linger", "--shot-tokens", "--code-stretch"];
const REPEATED: [&str; 4] = ["--exclude", "--exclude-ext", "--code-root", "--code-exclude"];

impl Arguments {
    pub fn parse(raw: &[String]) -> Result<Self> {
        let mut args = Self::default();
        let mut index = 0;
        while index < raw.len() {
            let argument = &raw[index];
            let repeated = REPEATED.contains(&argument.as_str());
            if VALUED.contains(&argument.as_str()) || repeated {
                let Some(value) = raw.get(index + 1) else { bail!("{argument} needs a value") };
                if repeated {
                    args.lists.entry(argument.clone()).or_default().push(value.clone());
                } else {
                    args.values.insert(argument.clone(), value.clone());
                }
                index += 2;
            } else if argument.starts_with("--") || argument == "-h" {
                args.flags.insert(argument.clone());
                index += 1;
            } else {
                args.positional.push(argument.clone());
                index += 1;
            }
        }
        Ok(args)
    }

    pub fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }

    pub fn list(&self, name: &str) -> Vec<String> {
        self.lists.get(name).cloned().unwrap_or_default()
    }

    pub fn number<T: std::str::FromStr>(&self, name: &str) -> Option<T> {
        self.values.get(name).and_then(|value| value.parse().ok())
    }

    pub fn index_folder(&self) -> PathBuf {
        self.values.get("--index").map(|path| folder_path(path)).unwrap_or_else(paths::default_index)
    }

    /// `--code`: the code index, not the main one.
    pub fn code(&self) -> bool {
        self.flag("--code")
    }

    pub fn store(&self) -> Result<Store> {
        Store::open(&self.index_folder(), if self.code() { store::CODE_FILE } else { store::MAIN_FILE })
    }

    pub fn options(&self) -> IndexOptions {
        let mut options = IndexOptions {
            code: self.code(),
            include_code: self.flag("--include-code"),
            include_datasets: self.flag("--include-datasets"),
            excluded_folders: self.list("--exclude").iter().map(|path| folder_path(path).to_string_lossy().into_owned()).collect(),
            excluded_extensions: IndexOptions::extensions(&self.list("--exclude-ext").join(",")),
            ..IndexOptions::default()
        };
        if let Some(tokens) = self.number("--shot-tokens") {
            options.screenshot_budget = tokens;
        }
        if let Some(characters) = self.number("--code-stretch") {
            options.code_stretch = characters;
        }
        options
    }

    pub fn folders(&self) -> Result<Vec<PathBuf>> {
        if self.positional.is_empty() {
            bail!("which folders? e.g. recall index ~/RecallTestbed/Acme");
        }
        self.positional
            .iter()
            .map(|path| {
                let folder = folder_path(path);
                if !is_folder(&folder) {
                    bail!("not a folder: {path}");
                }
                if !is_readable(&folder) {
                    bail!("can't read {path} (no permission?)");
                }
                Ok(folder)
            })
            .collect()
    }

    /// Dev knobs for A/B runs in evals/.
    pub fn z_gate(&self) -> f64 {
        self.number("--zgate").unwrap_or(2.0)
    }

    /// None: the index's own (`searcher`).
    pub fn word_weight(&self) -> Option<f64> {
        self.number("--word-weight")
    }

    pub fn kinds(&self) -> Result<Option<HashSet<FileKind>>> {
        let Some(list) = self.values.get("--kind") else { return Ok(None) };
        list.split(',')
            .map(|name| {
                FileKind::parse(name.trim()).ok_or_else(|| {
                    anyhow::anyhow!("unknown kind {name}; use screenshot, image, pdf, doc, audio, video")
                })
            })
            .collect::<Result<HashSet<_>>>()
            .map(Some)
    }
}

pub fn folder_path(path: &str) -> PathBuf {
    paths::standardize(&paths::expand(path))
}

pub fn is_folder(path: &Path) -> bool {
    path.is_dir()
}

/// The folder can be listed. One we may not read looks empty to a crawl, which must never be mistaken for
/// "everything in it was deleted".
pub fn is_readable(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok()
}

// MARK: The model

/// The embedding runtime: EmbeddingGemma 2 on llama.cpp, in this process.
pub fn make_embedder(text_only: bool, log: &Path) -> Result<Box<dyn Embedder>, WorkerError> {
    if let Some(socket) = std::env::var_os("RECALL_SHARED_SOCKET") {
        return Ok(Box::new(recall::shared::RemoteEmbedder::new(socket.into())));
    }
    Ok(Box::new(LlamaEmbedder::new(&LlamaModels::locate()?, text_only, Some(log))?))
}

/// `make_embedder`'s vector source, known without starting it.
pub fn embedder_source() -> Option<String> {
    Some(llama::model_info().vector_source())
}

// MARK: Output

/// One JSON object per line on stdout, flushed at once (the app reads it from a pipe). The app may be gone by now: a
/// closed pipe is ignored.
pub fn emit(fields: Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{fields}");
    let _ = out.flush();
}

pub fn tilde(path: &str) -> String {
    paths::tilde(Path::new(path))
}

pub fn pad(text: &str, width: usize) -> String {
    format!("{text:<width$}")
}

pub fn lpad(text: &str, width: usize) -> String {
    format!("{text:>width$}")
}

/// 12,345.
pub fn number(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

pub fn bytes(count: f64) -> String {
    match count {
        c if c < 1000.0 => format!("{} bytes", c as u64),
        c if c < 1e6 => format!("{:.0} KB", c / 1e3),
        c if c < 1e9 => format!("{:.1} MB", c / 1e6),
        c => format!("{:.2} GB", c / 1e9),
    }
}

pub fn duration(seconds: f64) -> String {
    if seconds < 90.0 {
        format!("{} s", seconds.round() as i64)
    } else if seconds < 5400.0 {
        format!("{} min", (seconds / 60.0).round() as i64)
    } else {
        format!("{:.1} h", seconds / 3600.0)
    }
}

pub fn clock(seconds: f64) -> String {
    let total = seconds as i64;
    if total >= 3600 {
        format!("{}:{:02}:{:02}", total / 3600, total / 60 % 60, total % 60)
    } else {
        format!("{}:{:02}", total / 60, total % 60)
    }
}

pub fn location(hit: &SearchHit) -> String {
    let Some(loc) = hit.loc else {
        return if hit.segment == SegmentKind::Ocr { "text in image".into() } else { String::new() };
    };
    let end = hit.loc_end.unwrap_or(loc);
    match hit.segment {
        SegmentKind::Page | SegmentKind::PageImage => format!("p. {}", loc as i64),
        SegmentKind::Audio => format!("at {} (sound)", clock(loc)),
        SegmentKind::Frame => format!("at {}", clock(loc)),
        SegmentKind::Chunk => format!("part {}", loc as i64),
        SegmentKind::Lines => format!("lines {}–{}", loc as i64, end as i64)
            + &hit.focus.map(|focus| format!(", opens at {}", focus as i64)).unwrap_or_default(),
        SegmentKind::Cells if loc == end => format!("cell {}", loc as i64),
        SegmentKind::Cells => format!("cells {}–{}", loc as i64, end as i64),
        SegmentKind::Ocr => "text in image".into(),
        _ => String::new(),
    }
}

pub fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
}

pub fn parent_folder(path: &str) -> String {
    Path::new(path).parent().map(|parent| parent.to_string_lossy().into_owned()).unwrap_or_default()
}

pub fn skipped_line(skipped: &BTreeMap<String, usize>) -> String {
    let mut rows: Vec<(&String, &usize)> = skipped.iter().collect();
    rows.sort_by(|a, b| b.1.cmp(a.1));
    rows.iter().map(|(reason, count)| format!("{} {reason}", number(**count))).collect::<Vec<_>>().join(" · ")
}

pub fn states_line(states: &BTreeMap<String, usize>) -> String {
    states.iter().map(|(state, count)| format!("{count} {state}")).collect::<Vec<_>>().join(" · ")
}

/// A hit as the app and `search --json` get it.
pub fn hit_row(hit: &SearchHit, full: bool) -> Value {
    let mut row = serde_json::json!({
        "path": hit.path, "kind": hit.kind.as_str(), "segment": hit.segment.as_str(), "score": hit.score,
    });
    if full {
        row["segment_id"] = hit.segment_id.into();
        row["modified"] = hit.modified.into();
        row["size"] = hit.size.into();
        row["info"] = serde_json::to_value(&hit.info).unwrap_or_default();
    }
    if let Some(z) = hit.z {
        row["z"] = z.into();
    }
    if let Some(cosine) = hit.cosine {
        row["cosine"] = cosine.into();
    }
    if let Some(rank) = hit.keyword_rank {
        row["keyword_rank"] = rank.into();
    }
    if let Some(loc) = hit.loc {
        row["loc"] = loc.into();
    }
    if let Some(end) = hit.loc_end {
        row["loc_end"] = end.into();
    }
    if let Some(focus) = hit.focus {
        row["focus"] = focus.into();
    }
    if let Some(excerpt) = &hit.excerpt {
        row["excerpt"] = excerpt.clone().into();
    }
    row
}
