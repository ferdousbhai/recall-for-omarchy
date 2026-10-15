//! EmbeddingGemma 2 on llama.cpp (CUDA, Vulkan or the CPU), in this process, through the C shim in
//! csrc/llama_shim.c: the image and audio encoders (mtmd) feed the text model, and its mean-pooled, L2-normalized
//! output is the vector. Audio past 30 s is dropped, as in the reference. One thread at a time; `close()` frees the
//! model (about 1 GB, or 0.25 GB text-only).

use std::ffi::{c_char, c_int, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::Instant;

use crate::embed::{Embedder, Embedding, WorkerError, WorkerInfo};
use crate::model::prompts;
use crate::model_files::{self, MEDIA, REPOSITORY, REVISION, TEXT};

#[repr(C)]
struct RawEmbedder {
    _private: [u8; 0],
}

extern "C" {
    fn recall_log_to(path: *const c_char);
    fn recall_log_line(text: *const c_char);
    fn recall_backend_init(folder: *const c_char, wanted: *const c_char);
    fn recall_backend_cpu();
    fn recall_device_name(out: *mut c_char, size: usize);
    fn recall_target_size(height: c_int, width: c_int, budget: c_int, out_height: *mut c_int, out_width: *mut c_int)
        -> c_int;
    fn recall_resize_bicubic(src: *const u8, width: c_int, height: c_int, to_width: c_int, to_height: c_int,
                             out: *mut u8);
    fn recall_open(text_path: *const c_char, media_path: *const c_char, max_tokens: c_int, error: *mut c_char,
                   error_size: usize) -> *mut RawEmbedder;
    fn recall_close(embedder: *mut RawEmbedder);
    fn recall_embedding_size(embedder: *const RawEmbedder) -> c_int;
    fn recall_embed_texts(embedder: *mut RawEmbedder, texts: *const *const c_char, count: c_int, out: *mut f32,
                          error: *mut c_char, error_size: usize) -> c_int;
    fn recall_embed_media(embedder: *mut RawEmbedder, path: *const c_char, image: c_int, budget: c_int, out: *mut f32,
                          error: *mut c_char, error_size: usize) -> c_int;
}

/// The image token budgets the vision encoder takes.
pub const BUDGETS: [u32; 5] = [70, 140, 280, 560, 1120];

/// Which vectors this pipeline makes (the model files, llama.cpp, the resizing, the prompts). It changes only when the
/// vectors do, and the reference vector test says when that is: every llama.cpp update must reproduce the reference
/// vectors, or come with a new version here (then every index embeds again). Indexes record this, not the llama.cpp
/// build, so an update that leaves the vectors alone keeps them (b11461 made version 1).
pub const VECTOR_VERSION: u32 = 1;

/// What every LlamaEmbedder reports (its `vector_source` goes into index fingerprints), known without loading it.
pub fn model_info() -> WorkerInfo {
    WorkerInfo {
        model: "google/embeddinggemma-2".into(),
        revision: REVISION.into(),
        runtime: format!("llama.cpp-vectors{VECTOR_VERSION}"),
        dtype: "text-q8_0+mmproj-q8_0".into(),
        dim: 768,
        text_only: false,
        load_seconds: None,
    }
}

/// Where the EmbeddingGemma 2 files are.
#[derive(Clone, Debug)]
pub struct LlamaModels {
    pub text: PathBuf,
    /// None while only the text part is there.
    pub media: Option<PathBuf>,
}

impl LlamaModels {
    /// The folders to look in, first match wins. `$RECALL_MODELS` is the app's choice and the only place it looks
    /// then: the developer's Hugging Face cache mustn't hide a download that's missing. Without it (the CLI): the
    /// app's data folder, then the Hugging Face cache.
    pub fn folders() -> Vec<PathBuf> {
        match std::env::var("RECALL_MODELS") {
            Ok(chosen) if !chosen.is_empty() => vec![PathBuf::from(chosen)],
            _ => vec![model_files::installed(), model_files::hugging_face_snapshot()],
        }
    }

    /// The first folder with the text part.
    pub fn locate() -> Result<Self, WorkerError> {
        let folders = Self::folders();
        for folder in &folders {
            let text = folder.join(TEXT.name);
            if !text.exists() {
                continue;
            }
            let media = folder.join(MEDIA.name);
            return Ok(Self { text, media: media.exists().then_some(media) });
        }
        let list: Vec<String> = folders.iter().map(|folder| folder.display().to_string()).collect();
        Err(WorkerError::NotFound(format!(
            "no {} in {}; get it with: hf download {REPOSITORY} {} {} --revision {REVISION}",
            TEXT.name, list.join(", "), TEXT.name, MEDIA.name
        )))
    }

    /// Both files are there, so indexing can run (checked without loading anything).
    pub fn complete() -> bool {
        Self::locate().map(|models| models.media.is_some()).unwrap_or(false)
    }
}

/// The llama.cpp release this was built with (scripts/build-llama.sh), for logs.
pub fn build_tag() -> String {
    env!("RECALL_LLAMA_BUILD_TAG").to_string()
}

/// Where the backend plugins are: `$RECALL_GGML_DIR`, else next to this executable (a package), else the build's own
/// Vendor/llama/lib.
fn backend_folder() -> PathBuf {
    if let Ok(folder) = std::env::var("RECALL_GGML_DIR") {
        if !folder.is_empty() {
            return folder.into();
        }
    }
    if let Some(here) = std::env::current_exe().ok().and_then(|exe| exe.canonicalize().ok()).and_then(|exe| exe.parent().map(Path::to_path_buf)) {
        let packaged = std::fs::read_dir(&here).map(|entries| {
            entries.flatten().any(|entry| entry.file_name().to_string_lossy().starts_with("libggml-cpu"))
        });
        if packaged.unwrap_or(false) {
            return here;
        }
    }
    PathBuf::from(env!("RECALL_LLAMA_LIB_DIR"))
}

fn cstring(text: &str) -> CString {
    CString::new(text.replace('\0', " ")).expect("no NUL")
}

fn path_cstring(path: &Path) -> CString {
    use std::os::unix::ffi::OsStrExt;
    CString::new(path.as_os_str().as_bytes()).unwrap_or_default()
}

static BACKEND: Once = Once::new();

/// llama.cpp's process-wide setup, once: the backend plugins this machine can run, and the device
/// (`RECALL_DEVICE=cpu|cuda|vulkan` picks one by hand).
fn backend() {
    BACKEND.call_once(|| {
        let folder = path_cstring(&backend_folder());
        let choice = std::env::var("RECALL_DEVICE").unwrap_or_default().to_lowercase();
        let wanted = cstring(if choice == "auto" { "" } else { &choice });
        unsafe { recall_backend_init(folder.as_ptr(), wanted.as_ptr()) };
    });
}

/// What the device is, for logs: "CUDA0 (NVIDIA GeForce RTX 4070)", "Vulkan0 (…)", "CPU".
pub fn device_name() -> String {
    backend();
    let mut buffer = [0 as c_char; 256];
    unsafe {
        recall_device_name(buffer.as_mut_ptr(), buffer.len());
        CStr::from_ptr(buffer.as_ptr()).to_string_lossy().into_owned()
    }
}

/// Runtime diagnostics without loading model weights.
pub fn diagnostics() -> serde_json::Value {
    let folder = backend_folder();
    let mut plugins: Vec<String> = std::fs::read_dir(&folder).into_iter().flatten().flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("libggml-") && name.ends_with(".so"))
        .collect();
    plugins.sort();
    serde_json::json!({"selected_device": device_name(), "requested_device": std::env::var("RECALL_DEVICE").unwrap_or_else(|_| "auto".into()),
                      "plugins": plugins, "runtime": build_tag(), "backend_directory": folder})
}

/// Where llama.cpp's warnings and errors go (stderr when None).
pub fn log_to(path: Option<&Path>) {
    let path = path.map(path_cstring);
    unsafe { recall_log_to(path.as_ref().map_or(std::ptr::null(), |path| path.as_ptr())) };
}

fn log_line(text: &str) {
    let text = cstring(text);
    unsafe { recall_log_line(text.as_ptr()) };
}

/// transformers' Gemma 4 image processor: the size an image is resized to for `budget` soft tokens, as
/// (height, width), or None when it's too thin.
pub fn target_size(height: u32, width: u32, budget: u32) -> Option<(u32, u32)> {
    let (mut out_height, mut out_width) = (0, 0);
    let found =
        unsafe { recall_target_size(height as c_int, width as c_int, budget as c_int, &mut out_height, &mut out_width) };
    (found != 0).then_some((out_height as u32, out_width as u32))
}

/// Pillow's bicubic resampling of packed RGB.
pub fn resize_bicubic(source: &[u8], width: u32, height: u32, to_width: u32, to_height: u32) -> Vec<u8> {
    assert_eq!(source.len(), (width * height * 3) as usize);
    let mut out = vec![0u8; (to_width * to_height * 3) as usize];
    unsafe {
        recall_resize_bicubic(source.as_ptr(), width as c_int, height as c_int, to_width as c_int, to_height as c_int,
                              out.as_mut_ptr())
    };
    out
}

fn normalized(mut vector: Vec<f32>) -> Vec<f32> {
    let sum: f64 = vector.iter().map(|value| f64::from(*value) * f64::from(*value)).sum();
    let scale = if sum > 0.0 { (1.0 / sum.sqrt()) as f32 } else { 0.0 };
    for value in &mut vector {
        *value *= scale;
    }
    vector
}

fn error_text(buffer: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buffer.as_ptr()).to_string_lossy().into_owned() }
}

pub struct LlamaEmbedder {
    info: WorkerInfo,
    raw: *mut RawEmbedder,
    size: usize,
}

// One thread at a time, but it may move between threads.
unsafe impl Send for LlamaEmbedder {}

impl LlamaEmbedder {
    /// `text_only`: just the text model (queries). `log`: where llama.cpp's warnings and errors go (stderr when None).
    pub fn new(models: &LlamaModels, text_only: bool, log: Option<&Path>) -> Result<Self, WorkerError> {
        let started = Instant::now();
        log_to(log);
        backend();
        let wanted = std::env::var("RECALL_DEVICE").unwrap_or_default().to_lowercase();
        if !["", "auto", "cpu", "cuda", "vulkan"].contains(&wanted.as_str()) {
            return Err(WorkerError::Failed(format!("invalid RECALL_DEVICE={wanted}; use auto, cpu, cuda or vulkan")));
        }
        let on_gpu = device_name() != "CPU";
        if ["cuda", "vulkan"].contains(&wanted.as_str()) && !on_gpu {
            return Err(WorkerError::Failed(format!("requested {wanted} GPU is unavailable; check the backend plugin and graphics driver, or use RECALL_DEVICE=cpu")));
        }
        let auto = wanted.is_empty() || wanted == "auto";
        if !text_only && models.media.is_none() {
            return Err(WorkerError::NotFound(format!("{} (images and audio) isn't downloaded yet", MEDIA.name)));
        }
        // One input must fit one micro-batch: 2048 tokens (the reference's cut), which also holds a 1120-token image or
        // a 750-token 30 s clip. Queries are short, so the query encoder reserves less and cuts at 512.
        let max_tokens = if text_only && std::env::var_os("RECALL_SHARED_INFERENCE").is_none() { 512 } else { 2048 };
        let text = path_cstring(&models.text);
        let media = if text_only { None } else { models.media.as_deref().map(path_cstring) };
        let mut error = [0 as c_char; 512];
        let raw = unsafe {
            recall_open(text.as_ptr(), media.as_ref().map_or(std::ptr::null(), |media| media.as_ptr()), max_tokens,
                        error.as_mut_ptr(), error.len())
        };
        if raw.is_null() && on_gpu && auto {
            log_line("GPU initialization failed; retrying on CPU");
            unsafe { recall_backend_cpu() };
            return Self::new(models, text_only, log);
        }
        if raw.is_null() {
            return Err(WorkerError::Failed(error_text(&error)));
        }
        let size = unsafe { recall_embedding_size(raw) } as usize;
        let mut info = model_info();
        info.dim = size;
        info.text_only = text_only;
        let mut embedder = Self { info, raw, size };
        // The first calls build the GPU pipelines: pay that now (the panel is opening).
        for _ in 0..2 {
            if let Err(error) = embedder.embed_texts(&[prompts::query("warm up")]) {
                if on_gpu && auto {
                    embedder.close();
                    log_line("GPU warmup failed; retrying on CPU");
                    unsafe { recall_backend_cpu() };
                    return Self::new(models, text_only, log);
                }
                return Err(error);
            }
        }
        let seconds = (started.elapsed().as_secs_f64() * 100.0).round() / 100.0;
        embedder.info.load_seconds = Some(seconds);
        log_line(&format!(
            "ready in {seconds} s ({}, llama.cpp {}, vectors {VECTOR_VERSION}, on {})",
            if text_only { "text-only" } else { "full" },
            build_tag(),
            device_name()
        ));
        Ok(embedder)
    }

    fn embed_media(&mut self, path: &str, image: bool, budget: u32) -> Embedding {
        let mut out = vec![0f32; self.size];
        let mut error = [0 as c_char; 512];
        let path = cstring(path);
        let status = unsafe {
            recall_embed_media(self.raw, path.as_ptr(), image as c_int, budget as c_int, out.as_mut_ptr(),
                               error.as_mut_ptr(), error.len())
        };
        if status == 0 {
            Embedding { vector: Some(normalized(out)), error: None }
        } else {
            // An unreadable file must not sink its neighbours.
            Embedding { vector: None, error: Some(error_text(&error).chars().take(300).collect()) }
        }
    }

    fn check_media(&self) -> Result<(), WorkerError> {
        if self.raw.is_null() || self.info.text_only {
            return Err(WorkerError::Failed("this embedder was started text-only".into()));
        }
        Ok(())
    }
}

impl Embedder for LlamaEmbedder {
    fn info(&self) -> &WorkerInfo {
        &self.info
    }

    fn embed_texts(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, WorkerError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        if self.raw.is_null() {
            return Err(WorkerError::Failed("the model is closed".into()));
        }
        let owned: Vec<CString> = texts.iter().map(|text| cstring(text)).collect();
        let pointers: Vec<*const c_char> = owned.iter().map(|text| text.as_ptr()).collect();
        let mut out = vec![0f32; self.size * texts.len()];
        let mut error = [0 as c_char; 512];
        let status = unsafe {
            recall_embed_texts(self.raw, pointers.as_ptr(), texts.len() as c_int, out.as_mut_ptr(), error.as_mut_ptr(),
                               error.len())
        };
        if status != 0 {
            return Err(WorkerError::Failed(error_text(&error)));
        }
        Ok(out.chunks_exact(self.size).map(|row| normalized(row.to_vec())).collect())
    }

    fn embed_images(&mut self, paths: &[String], budget: u32) -> Result<Vec<Embedding>, WorkerError> {
        self.check_media()?;
        if !BUDGETS.contains(&budget) {
            return Err(WorkerError::Failed(format!("budget must be one of {BUDGETS:?}")));
        }
        Ok(paths.iter().map(|path| self.embed_media(path, true, budget)).collect())
    }

    fn embed_audio(&mut self, paths: &[String]) -> Result<Vec<Embedding>, WorkerError> {
        self.check_media()?;
        Ok(paths.iter().map(|path| self.embed_media(path, false, 0)).collect())
    }

    fn close(&mut self) {
        if !self.raw.is_null() {
            unsafe { recall_close(self.raw) };
            self.raw = std::ptr::null_mut();
        }
    }
}

impl Drop for LlamaEmbedder {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // llama.cpp has to see the pixels the reference sees: transformers' resize target, then PIL's bicubic.

    #[test]
    fn target_size_fills_the_budget() {
        // A 1440×900 screenshot and a 768×768 photo, at the budgets the indexer uses.
        assert_eq!(target_size(900, 1440, 280), Some((624, 1008)));
        assert_eq!(target_size(900, 1440, 140), Some((432, 672)));
        assert_eq!(target_size(900, 1440, 70), Some((288, 480)));
        assert_eq!(target_size(768, 768, 280), Some((768, 768)));
        assert_eq!(target_size(768, 768, 140), Some((528, 528)));
        // Small images grow to fill it, as in the reference (mtmd alone would leave them small).
        assert_eq!(target_size(300, 300, 140), Some((528, 528)));
    }

    #[test]
    fn target_size_of_a_thin_image() {
        // One side rounds to zero: it gets one 48 px row, the other side capped at the budget.
        assert_eq!(target_size(10, 4000, 140), Some((48, 6720)));
    }

    /// A 7×5 image resized by PIL (Image.resize, BICUBIC).
    const SOURCE: [u8; 105] = [0, 115, 230, 3, 118, 233, 6, 121, 236, 9, 124, 239, 12, 127, 242, 15, 130, 245, 18,
        133, 248, 77, 192, 51, 80, 195, 54, 83, 198, 57, 86, 201, 60, 89, 204, 63, 92, 207, 66, 95, 210, 69, 154, 13,
        128, 157, 16, 131, 160, 19, 134, 163, 22, 137, 166, 25, 140, 169, 28, 143, 172, 31, 146, 231, 90, 205, 234, 93,
        208, 237, 96, 211, 240, 99, 214, 243, 102, 217, 246, 105, 220, 249, 108, 223, 52, 167, 26, 55, 170, 29, 58,
        173, 32, 61, 176, 35, 64, 179, 38, 67, 182, 41, 70, 185, 44];

    fn resize(to_width: u32, to_height: u32) -> Vec<u8> {
        resize_bicubic(&SOURCE, 7, 5, to_width, to_height)
    }

    #[test]
    fn shrink_matches_pillow() {
        let pil = [28, 151, 152, 33, 156, 157, 39, 162, 163, 44, 167, 168, 165, 68, 129, 170, 73, 134, 176, 79,
            140, 181, 84, 145, 132, 133, 106, 137, 138, 111, 143, 144, 117, 148, 149, 122];
        assert_eq!(resize(4, 3), pil);
    }

    #[test]
    fn grow_matches_pillow() {
        let pil = [0, 112, 237, 0, 113, 238, 1, 116, 241, 3, 118, 243, 5, 120, 245, 7, 122, 247, 9, 124, 249,
            11, 126, 251, 14, 129, 254, 15, 130, 255, 55, 187, 85, 56, 188, 86, 59, 191, 89, 61, 193, 91, 63, 195, 93,
            65, 197, 95, 67, 199, 97, 69, 201, 99, 72, 204, 102, 73, 205, 103, 122, 82, 83, 123, 83, 84, 126, 86, 87,
            128, 88, 89, 130, 90, 91, 132, 92, 93, 134, 94, 95, 136, 96, 97, 139, 99, 100, 140, 100, 101, 199, 27, 173,
            200, 28, 174, 203, 31, 177, 205, 33, 179, 207, 35, 181, 209, 37, 183, 211, 39, 185, 213, 41, 187, 216, 44,
            190, 217, 45, 191, 197, 112, 171, 198, 113, 172, 201, 116, 175, 203, 118, 177, 205, 120, 179, 207, 122, 181,
            209, 124, 183, 211, 126, 185, 214, 129, 188, 215, 130, 189, 45, 170, 19, 46, 171, 20, 49, 174, 23, 51, 176,
            25, 53, 178, 27, 55, 180, 29, 57, 182, 31, 59, 184, 33, 62, 187, 36, 63, 188, 37];
        assert_eq!(resize(10, 6), pil);
    }

    #[test]
    fn one_axis_matches_pillow() {
        let pil = [74, 120, 137, 77, 123, 140, 80, 126, 143, 83, 129, 146, 86, 132, 149, 89, 135, 152, 92, 138,
            155, 153, 100, 119, 156, 103, 122, 159, 106, 125, 162, 109, 128, 165, 112, 131, 168, 115, 134, 171, 118,
            137];
        assert_eq!(resize(7, 2), pil);
        assert_eq!(resize(7, 5), SOURCE);
    }
}
