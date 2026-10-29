//! Running the system's tools that read files: vips or ffmpeg for pictures, poppler for PDFs,
//! catdoc or antiword for old .doc files, ffmpeg for sound and video.

use std::env;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

/// Why a file couldn't be read.
#[derive(Clone, Debug)]
pub enum ExtractError {
    Unreadable(String),
    TooSmall(u32, u32),
    Locked,
    Empty(String),
    /// Not for the index after all, once read (code that a program made, or that holds a private key).
    LeftOut(String),
}

impl fmt::Display for ExtractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable(why) => write!(f, "unreadable: {why}"),
            Self::TooSmall(width, height) => write!(f, "small image {width}×{height}"),
            Self::Locked => f.write_str("password-protected"),
            Self::Empty(why) | Self::LeftOut(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for ExtractError {}

pub type Result<T> = std::result::Result<T, ExtractError>;

/// `name` on $PATH, or in the usual places (a helper launched by a desktop session may get a short PATH).
pub fn path(name: &str) -> Option<PathBuf> {
    let paths = env::var("PATH").unwrap_or_default();
    let found = paths
        .split(':')
        .filter(|dir| !dir.is_empty())
        .chain(["/usr/bin", "/usr/local/bin", "/bin", "/snap/bin"])
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| is_executable(candidate));
    found
}

pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata().map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

pub struct Output {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.status == 0
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// The last line of a tool's complaint, for an error message.
    pub fn complaint(&self) -> String {
        self.stderr.lines().rev().find(|line| !line.trim().is_empty()).map(String::from)
            .unwrap_or_else(|| format!("exit status {}", self.status))
    }
}

/// Runs `name` and collects what it prints (both pipes at once, so neither fills up and stalls the tool).
pub fn run<S: AsRef<std::ffi::OsStr>>(name: &str, arguments: &[S]) -> Result<Output> {
    let executable =
        path(name).ok_or_else(|| ExtractError::Unreadable(format!("needs {name}, which isn't installed")))?;
    let mut child = Command::new(executable)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| ExtractError::Unreadable(format!("can't run {name}: {error}")))?;
    let mut errors = child.stderr.take().expect("piped");
    let drain = thread::spawn(move || {
        let mut text = Vec::new();
        let _ = errors.read_to_end(&mut text);
        text
    });
    let mut stdout = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_end(&mut stdout);
    }
    let stderr = drain.join().unwrap_or_default();
    let status = child.wait().map(|status| status.code().unwrap_or(-1)).unwrap_or(-1);
    Ok(Output { status, stdout, stderr: String::from_utf8_lossy(&stderr).trim().to_string() })
}
