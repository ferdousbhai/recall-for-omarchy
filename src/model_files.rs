//! The EmbeddingGemma 2 files the engine runs: ggml-org's Q8_0 GGUF conversion at one pinned revision of the repo
//! (made from google/embeddinggemma-2 at 914f7f8). Never the F16 files: the model's activations overflow float16.

use std::env;
use std::ffi::OsString;
use std::fs::{self, File as FsFile, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

use crate::paths;

pub const REPOSITORY: &str = "ggml-org/embeddinggemma-2-GGUF";
pub const REVISION: &str = "bfcd298762cc34d0357ece5ebdd31791a3a374d8";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelFile {
    pub name: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

impl ModelFile {
    /// Hugging Face's address for this file at the pinned revision.
    pub fn url(&self) -> String {
        format!("https://huggingface.co/{REPOSITORY}/resolve/{REVISION}/{}", self.name)
    }
}

/// The text model (310 MB): all that queries need.
pub const TEXT: ModelFile = ModelFile {
    name: "embeddinggemma-2-Q8_0.gguf",
    bytes: 309_855_456,
    sha256: "2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135",
};
/// The image and audio encoders (555 MB).
pub const MEDIA: ModelFile = ModelFile {
    name: "mmproj-embeddinggemma-2-Q8_0.gguf",
    bytes: 554_821_024,
    sha256: "c4a8a52691ecef40618438928bdf9e68379b854e24166f292592353db0aab64f",
};
/// In download order.
pub const ALL: [ModelFile; 2] = [TEXT, MEDIA];

/// The pre-0.7 default folder. Installs move out of it (`adopt_legacy`); nothing new is written there.
pub fn legacy_folder() -> PathBuf {
    paths::support().join("Models")
}

/// The Hugging Face hub cache, by the precedence llama.cpp and `hf` use. `LLAMA_CACHE` is llama.cpp's own and not read.
pub fn hub_cache() -> PathBuf {
    hub_cache_from(|name| env::var_os(name), &paths::home())
}

fn hub_cache_from(var: impl Fn(&str) -> Option<OsString>, home: &Path) -> PathBuf {
    let set = |name: &str| var(name).filter(|value| !value.is_empty());
    let location = |value: OsString| match value.to_str() {
        Some(text) => paths::standardize(&paths::expand(text)),
        None => PathBuf::from(value),
    };
    set("HF_HUB_CACHE")
        .or_else(|| set("HUGGINGFACE_HUB_CACHE"))
        .map(location)
        .or_else(|| set("HF_HOME").map(|home| location(home).join("hub")))
        // The XDG base directory spec says to ignore a relative value.
        .or_else(|| set("XDG_CACHE_HOME").filter(|path| Path::new(path).is_absolute()).map(|cache| location(cache).join("huggingface/hub")))
        .unwrap_or_else(|| home.join(".cache/huggingface/hub"))
}

fn repo_folder() -> String {
    format!("models--{}", REPOSITORY.replace('/', "--"))
}

/// The pinned revision's folder in the hub cache: the default place for the model, whatever `hf` or Recall put there.
pub fn hub_snapshot() -> PathBuf {
    hub_cache().join(repo_folder()).join("snapshots").join(REVISION)
}

/// The repository folder, when `folder` is a snapshot of this model in some hub cache.
pub fn snapshot_repo(folder: &Path) -> Option<&Path> {
    let snapshots = folder.parent()?;
    let repo = snapshots.parent()?;
    (snapshots.file_name()? == "snapshots" && repo.file_name()? == repo_folder().as_str()).then_some(repo)
}

/// Where the model lives when `models_directory` isn't customized: the hub snapshot, except while the legacy folder
/// still holds a file the snapshot lacks, so that search keeps working until `adopt_legacy` has moved them all.
pub fn default_folder() -> PathBuf {
    default_among(&ALL, &legacy_folder(), &hub_snapshot())
}

fn default_among(files: &[ModelFile], legacy: &Path, snapshot: &Path) -> PathBuf {
    let moving = |file: &ModelFile| {
        legacy.join(file.name).metadata().is_ok_and(|found| found.len() == file.bytes) && !snapshot.join(file.name).exists()
    };
    let folder = if files.iter().any(moving) { legacy } else { snapshot };
    folder.to_path_buf()
}

/// Saved settings that mean "the default": the legacy folder (setup and Settings used to save it along with everything
/// else) and any revision's snapshot in the current hub cache.
pub fn follows_default(saved: &str) -> bool {
    let path = paths::standardize(Path::new(saved));
    path == legacy_folder() || snapshot_repo(&path).is_some_and(|repo| repo == hub_cache().join(repo_folder()))
}

/// The folder a saved `models_directory` stands for. A snapshot of this model follows the pin, so a release that pins a
/// new revision never downloads into a stale one, including in a hub cache elsewhere. Any other folder is taken as is.
pub fn resolve_setting(saved: &str) -> PathBuf {
    if follows_default(saved) {
        return default_folder();
    }
    let path = paths::standardize(Path::new(saved));
    match snapshot_repo(&path) {
        Some(repo) => repo.join("snapshots").join(REVISION),
        None => path,
    }
}

/// `file` in the Hugging Face cache, when it's there at the right size (its checksum is for the caller to check).
pub fn cached_copy(file: &ModelFile) -> Option<PathBuf> {
    let path = hub_snapshot().join(file.name).canonicalize().ok()?;
    (path.metadata().ok()?.len() == file.bytes).then_some(path)
}

/// The files that aren't in `folder` yet. A customized folder only ever gets verified files, and `prepare` checks a
/// snapshot, which other tools can write to, so being there is enough.
pub fn missing(folder: &Path) -> Vec<ModelFile> {
    ALL.into_iter().filter(|file| !folder.join(file.name).exists()).collect()
}

/// How much of `file` has arrived, for progress.
pub fn received(folder: &Path, file: &ModelFile) -> u64 {
    let partial = match snapshot_repo(folder) {
        Some(repo) => repo.join("blobs").join(format!("{}.incomplete", file.sha256)),
        None => folder.join(format!("{}.part", file.name)),
    };
    folder.join(file.name).metadata().or_else(|_| partial.metadata()).map_or(0, |found| found.len().min(file.bytes))
}

/// Run before using a folder. A snapshot gets its existing files checked, and the default snapshot takes over the legacy
/// folder's. A customized plain folder is left alone.
pub fn prepare(folder: &Path) -> io::Result<()> {
    if folder == legacy_folder() || folder == hub_snapshot() {
        check_snapshot(&hub_snapshot(), &ALL)?;
        adopt(&ALL, &legacy_folder(), &hub_snapshot())
    } else if snapshot_repo(folder).is_some() {
        check_snapshot(folder, &ALL)
    } else {
        Ok(())
    }
}

/// The cache is shared, so what's in a snapshot is checked rather than trusted. A wrong entry stops being used: its link
/// goes, and any blob stays for `hf cache` to manage. A regular file is renamed, not deleted.
fn check_snapshot(snapshot: &Path, files: &[ModelFile]) -> io::Result<()> {
    let mut found = false;
    for file in files {
        let path = snapshot.join(file.name);
        if !path.exists() {
            continue;
        }
        if verified(&path, file)? {
            found = true;
        } else if path.is_symlink() {
            eprintln!("Models: {} is not the pinned file; ignoring that link", path.display());
            fs::remove_file(&path)?;
        } else {
            eprintln!("Models: {} is not the pinned file; renaming it", path.display());
            fs::rename(&path, path.with_file_name(format!("{}.invalid", file.name)))?;
        }
    }
    match snapshot_repo(snapshot) {
        // llama.cpp finds a cached repository through its refs, and `hf download --revision <commit>` writes none.
        Some(repo) if found => ensure_ref(repo, snapshot),
        _ => Ok(()),
    }
}

/// Move the legacy folder's files into the snapshot. Every file is published before any is removed, so one of the two
/// folders is complete throughout; a failure leaves the legacy folder in use. A file that fails its checksum is renamed
/// and downloaded afresh.
fn adopt(files: &[ModelFile], legacy: &Path, snapshot: &Path) -> io::Result<()> {
    let mut moved = Vec::new();
    for file in files {
        let old = legacy.join(file.name);
        if !old.exists() {
            continue;
        }
        if !snapshot.join(file.name).exists() {
            let bring = |_: &ModelFile, partial: &Path| link_or_copy(&old, partial);
            match publish(snapshot, file, &bring) {
                Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                    eprintln!("Models: {} is not the pinned file; renaming it", old.display());
                    fs::rename(&old, old.with_file_name(format!("{}.invalid", file.name)))?;
                    continue;
                }
                result => result?,
            }
        }
        moved.push(old);
    }
    for old in moved {
        fs::remove_file(old)?;
    }
    for file in files {
        let _ = fs::remove_file(legacy.join(format!("{}.part", file.name)));
    }
    let _ = fs::remove_file(legacy.join(".download.lock"));
    // Only succeeds when nothing else is in there.
    let _ = fs::remove_dir(legacy);
    Ok(())
}

/// Download `file` into `folder` from the pinned URL, unless it's there by the time the lock is free.
pub fn install(folder: &Path, file: &ModelFile) -> io::Result<()> {
    install_with(folder, file, &curl)
}

type Transfer<'a> = &'a dyn Fn(&ModelFile, &Path) -> io::Result<()>;

fn install_with(folder: &Path, file: &ModelFile, transfer: Transfer) -> io::Result<()> {
    if snapshot_repo(folder).is_some() {
        publish(folder, file, transfer)
    } else {
        install_flat(folder, file, transfer)
    }
}

/// A real hub layout: the verified bytes become `blobs/<sha256>`, then a relative symlink in the snapshot appears
/// atomically. `blobs/<sha256>.incomplete` and `.locks/<repo>/<sha256>.lock` are the names `hf` uses for the same file
/// (a Git LFS file's etag is its SHA-256), so the two take turns and resume each other's downloads. A blob already there,
/// for example from another revision, is reused.
fn publish(snapshot: &Path, file: &ModelFile, transfer: Transfer) -> io::Result<()> {
    let repo = snapshot_repo(snapshot).ok_or_else(|| io::Error::other("Not a model snapshot"))?;
    make_dir(repo)?;
    let blobs = repo.join("blobs");
    make_dir(&blobs)?;
    make_dir(&repo.join("snapshots"))?;
    make_dir(snapshot)?;
    let locks = repo.parent().unwrap_or(repo).join(".locks").join(repo_folder());
    make_dir(repo.parent().unwrap_or(repo).join(".locks").as_path())?;
    make_dir(&locks)?;
    let _lock = lock(&locks.join(format!("{}.lock", file.sha256)))?;
    let entry = snapshot.join(file.name);
    if !entry.exists() {
        let blob = blobs.join(file.sha256);
        if !blob.exists() || !verified(&blob, file)? {
            let partial = blobs.join(format!("{}.incomplete", file.sha256));
            transfer(file, &partial)?;
            accept(&partial, file, &blob)?;
        }
        let link = snapshot.join(format!(".{}.{}", file.name, uuid::Uuid::new_v4()));
        symlink(format!("../../blobs/{}", file.sha256), &link)?;
        if let Err(error) = fs::rename(&link, &entry) {
            let _ = fs::remove_file(&link);
            return Err(error);
        }
    }
    ensure_ref(repo, snapshot)
}

/// Point `refs/main` at the snapshot only if no tool has said where `main` is: Recall never moves a ref, and never reads
/// one, so the pin can't drift through it.
fn ensure_ref(repo: &Path, snapshot: &Path) -> io::Result<()> {
    let refs = repo.join("refs");
    let main = refs.join("main");
    if main.symlink_metadata().is_ok() {
        return Ok(());
    }
    let commit = snapshot.file_name().ok_or_else(|| io::Error::other("Not a model snapshot"))?;
    make_dir(&refs)?;
    let temporary = refs.join(format!(".main.{}", uuid::Uuid::new_v4()));
    fs::write(&temporary, commit.as_encoded_bytes())?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o644))?;
    // A link, unlike a rename, can't replace a ref that appeared meanwhile.
    let linked = fs::hard_link(&temporary, &main);
    fs::remove_file(&temporary)?;
    match linked {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => Err(error),
        _ => Ok(()),
    }
}

/// A customized folder holds plain files, and a copy in the hub cache is linked rather than downloaded again.
fn install_flat(folder: &Path, file: &ModelFile, transfer: Transfer) -> io::Result<()> {
    fs::create_dir_all(folder)?;
    let _lock = lock(&folder.join(".download.lock"))?;
    let destination = folder.join(file.name);
    if destination.is_file() {
        return Ok(());
    }
    let temporary = folder.join(format!("{}.part", file.name));
    if let Some(cached) = cached_copy(file) {
        link_or_copy(&cached, &temporary)?;
        match accept(&temporary, file, &destination) {
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {}
            result => return result,
        }
    }
    transfer(file, &temporary)?;
    accept(&temporary, file, &destination)
}

/// Give `temporary` its final name only if it's the pinned file; otherwise it goes. The weights are public, and readable
/// by whoever can read the folder, whatever the service's umask.
fn accept(temporary: &Path, file: &ModelFile, destination: &Path) -> io::Result<()> {
    if !verified(temporary, file)? {
        fs::remove_file(temporary)?;
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Model checksum verification failed"));
    }
    fs::set_permissions(temporary, fs::Permissions::from_mode(0o644))?;
    fs::rename(temporary, destination)
}

fn verified(path: &Path, file: &ModelFile) -> io::Result<bool> {
    Ok(path.metadata()?.len() == file.bytes && sha256(path)? == file.sha256)
}

fn link_or_copy(source: &Path, temporary: &Path) -> io::Result<()> {
    let source = source.canonicalize()?;
    match fs::remove_file(temporary) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    if fs::hard_link(&source, temporary).is_err() {
        fs::copy(&source, temporary)?;
    }
    Ok(())
}

fn make_dir(path: &Path) -> io::Result<()> {
    if !path.is_dir() {
        fs::create_dir_all(path)?;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

/// Held until the file is dropped.
fn lock(path: &Path) -> io::Result<FsFile> {
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
    // Another user sharing the cache has to be able to open it too; the service's umask is private.
    let _ = file.set_permissions(fs::Permissions::from_mode(0o644));
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

fn curl(file: &ModelFile, partial: &Path) -> io::Result<()> {
    let status = Command::new("curl")
        .args(["-fL", "--retry", "3", "--connect-timeout", "15", "--speed-limit", "1024", "--speed-time", "60", "-C", "-", "-o"])
        .arg(partial)
        .arg(file.url())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("Command failed: {status}")))
    }
}

/// The SHA-256 of a file as lowercase hex, read in 8 MB pieces.
pub fn sha256(path: &Path) -> std::io::Result<String> {
    let mut file = FsFile::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 8 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_files_in_pieces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let bytes: Vec<u8> = (0..(9u32 << 20)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &bytes).unwrap();
        let expected: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(sha256(&path).unwrap(), expected);
    }

    #[test]
    fn missing_files_in_download_order() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(missing(dir.path()), vec![TEXT, MEDIA]);
        std::fs::write(dir.path().join(TEXT.name), b"x").unwrap();
        assert_eq!(missing(dir.path()), vec![MEDIA]);
    }

    fn spec(name: &'static str, bytes: &[u8]) -> ModelFile {
        let hash: String = Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
        ModelFile { name, bytes: bytes.len() as u64, sha256: Box::leak(hash.into_boxed_str()) }
    }

    fn cached_snapshot(hub: &Path) -> PathBuf {
        hub.join(repo_folder()).join("snapshots").join(REVISION)
    }

    fn fails(_: &ModelFile, _: &Path) -> io::Result<()> {
        panic!("nothing should be downloaded")
    }

    #[test]
    fn hub_cache_precedence() {
        let all = [
            ("HF_HUB_CACHE", "/a"),
            ("HUGGINGFACE_HUB_CACHE", "/b"),
            ("HF_HOME", "/c"),
            ("XDG_CACHE_HOME", "/d"),
        ];
        let with = |set: &[(&str, &str)]| {
            let set = set.to_vec();
            hub_cache_from(move |name| set.iter().find(|(key, _)| *key == name).map(|(_, value)| value.into()), Path::new("/home/me"))
        };
        assert_eq!(with(&all), Path::new("/a"));
        assert_eq!(with(&all[1..]), Path::new("/b"));
        assert_eq!(with(&all[2..]), Path::new("/c/hub"));
        assert_eq!(with(&all[3..]), Path::new("/d/huggingface/hub"));
        assert_eq!(with(&[]), Path::new("/home/me/.cache/huggingface/hub"));
        // Empty is unset; a relative XDG value is invalid by its spec; paths come out tidy.
        assert_eq!(with(&[("HF_HUB_CACHE", ""), ("HF_HOME", "/c/")]), Path::new("/c/hub"));
        assert_eq!(with(&[("XDG_CACHE_HOME", "relative")]), Path::new("/home/me/.cache/huggingface/hub"));
        assert_eq!(with(&[("HF_HUB_CACHE", "/a/./b/../c")]), Path::new("/a/c"));
    }

    #[test]
    fn recognizes_snapshots_of_this_model_only() {
        let repo = PathBuf::from("/hub").join(repo_folder());
        assert_eq!(snapshot_repo(&repo.join("snapshots/abc")), Some(repo.as_path()));
        assert_eq!(snapshot_repo(&repo.join("blobs/abc")), None);
        assert_eq!(snapshot_repo(Path::new("/hub/models--other--repo/snapshots/abc")), None);
        assert_eq!(snapshot_repo(Path::new("/home/me/my-models")), None);
    }

    #[test]
    fn publishes_a_real_hub_layout() {
        let hub = tempfile::tempdir().unwrap();
        let snapshot = cached_snapshot(hub.path());
        let file = spec("model.gguf", b"weights");
        install_with(&snapshot, &file, &|_, partial| fs::write(partial, b"weights")).unwrap();

        let repo = hub.path().join(repo_folder());
        let blob = repo.join("blobs").join(file.sha256);
        assert_eq!(fs::read(&blob).unwrap(), b"weights");
        assert!(blob.symlink_metadata().unwrap().is_file());
        assert_eq!(blob.metadata().unwrap().permissions().mode() & 0o777, 0o644);
        let entry = snapshot.join(file.name);
        assert_eq!(fs::read_link(&entry).unwrap(), Path::new("../../blobs").join(file.sha256));
        assert_eq!(fs::read(&entry).unwrap(), b"weights");
        assert_eq!(fs::read_to_string(repo.join("refs/main")).unwrap(), REVISION);
        assert_eq!(repo.join("refs/main").metadata().unwrap().permissions().mode() & 0o777, 0o644);
        assert!(hub.path().join(".locks").join(repo_folder()).join(format!("{}.lock", file.sha256)).exists());
        // Nothing temporary is left beside them.
        let names = |path: &Path| fs::read_dir(path).unwrap().map(|e| e.unwrap().file_name()).collect::<Vec<_>>();
        assert_eq!(names(&repo.join("blobs")).len(), 1);
        assert_eq!(names(&snapshot).len(), 1);
        assert_eq!(names(&repo.join("refs")).len(), 1);
    }

    #[test]
    fn a_bad_download_is_never_published() {
        let hub = tempfile::tempdir().unwrap();
        let snapshot = cached_snapshot(hub.path());
        let file = spec("model.gguf", b"weights");
        let error = install_with(&snapshot, &file, &|_, partial| fs::write(partial, b"tampers")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!snapshot.join(file.name).exists());
        assert!(!hub.path().join(repo_folder()).join("blobs").join(file.sha256).exists());
        assert!(!hub.path().join(repo_folder()).join("refs").exists());
    }

    #[test]
    fn a_new_revision_gets_its_own_snapshot_and_reuses_blobs() {
        let hub = tempfile::tempdir().unwrap();
        let repo = hub.path().join(repo_folder());
        let file = spec("model.gguf", b"weights");
        install_with(&repo.join("snapshots/old"), &file, &|_, partial| fs::write(partial, b"weights")).unwrap();
        install_with(&repo.join("snapshots/new"), &file, &fails).unwrap();
        assert_eq!(fs::read(repo.join("snapshots/new/model.gguf")).unwrap(), b"weights");
        assert_eq!(fs::read(repo.join("snapshots/old/model.gguf")).unwrap(), b"weights");
        assert_eq!(fs::read_to_string(repo.join("refs/main")).unwrap(), "old", "an existing ref is never moved");
    }

    #[test]
    fn a_corrupt_blob_is_repaired_and_a_tools_ref_is_kept() {
        let hub = tempfile::tempdir().unwrap();
        let repo = hub.path().join(repo_folder());
        let snapshot = cached_snapshot(hub.path());
        let file = spec("model.gguf", b"weights");
        fs::create_dir_all(repo.join("blobs")).unwrap();
        fs::create_dir_all(repo.join("refs")).unwrap();
        fs::write(repo.join("refs/main"), "theirs").unwrap();
        fs::write(repo.join("blobs").join(file.sha256), b"damaged").unwrap();
        install_with(&snapshot, &file, &|_, partial| fs::write(partial, b"weights")).unwrap();
        assert_eq!(fs::read(snapshot.join(file.name)).unwrap(), b"weights");
        assert_eq!(fs::read_to_string(repo.join("refs/main")).unwrap(), "theirs");
    }

    #[test]
    fn checking_a_snapshot_drops_only_wrong_entries() {
        let hub = tempfile::tempdir().unwrap();
        let repo = hub.path().join(repo_folder());
        let snapshot = cached_snapshot(hub.path());
        let good = spec("good.gguf", b"good");
        let linked = spec("linked.gguf", b"linked");
        let copied = spec("copied.gguf", b"copied");
        fs::create_dir_all(repo.join("blobs")).unwrap();
        fs::create_dir_all(&snapshot).unwrap();
        // Blobs that live elsewhere, as content-addressed stores arrange it: the chain of links is fine.
        fs::write(hub.path().join("store"), b"good").unwrap();
        symlink(hub.path().join("store"), repo.join("blobs").join(good.sha256)).unwrap();
        symlink(format!("../../blobs/{}", good.sha256), snapshot.join(good.name)).unwrap();
        fs::write(repo.join("blobs/wrong"), b"wrong!").unwrap();
        symlink("../../blobs/wrong", snapshot.join(linked.name)).unwrap();
        fs::write(snapshot.join(copied.name), b"wrong!").unwrap();

        check_snapshot(&snapshot, &[good, linked, copied]).unwrap();
        assert_eq!(fs::read(snapshot.join(good.name)).unwrap(), b"good");
        assert!(!snapshot.join(linked.name).exists());
        assert_eq!(fs::read(repo.join("blobs/wrong")).unwrap(), b"wrong!", "the shared blob stays");
        assert!(!snapshot.join(copied.name).exists());
        assert_eq!(fs::read(snapshot.join("copied.gguf.invalid")).unwrap(), b"wrong!");
        assert_eq!(fs::read_to_string(repo.join("refs/main")).unwrap(), REVISION, "discoverable by llama.cpp");
    }

    #[test]
    fn checking_an_empty_snapshot_writes_nothing() {
        let hub = tempfile::tempdir().unwrap();
        check_snapshot(&cached_snapshot(hub.path()), &[spec("model.gguf", b"weights")]).unwrap();
        assert_eq!(fs::read_dir(hub.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_legacy_install_moves_without_a_download_and_stays_usable_until_it_has() {
        let base = tempfile::tempdir().unwrap();
        let legacy = base.path().join("Models");
        let snapshot = cached_snapshot(&base.path().join("hub"));
        let text = spec("text.gguf", b"text");
        let media = spec("media.gguf", b"media");
        let files = [text, media];
        fs::create_dir_all(&legacy).unwrap();
        for (file, bytes) in [(text, &b"text"[..]), (media, &b"media"[..])] {
            fs::write(legacy.join(file.name), bytes).unwrap();
            fs::set_permissions(legacy.join(file.name), fs::Permissions::from_mode(0o600)).unwrap();
        }
        fs::write(legacy.join(".download.lock"), "").unwrap();
        fs::write(legacy.join("media.gguf.part"), "partial").unwrap();
        fs::write(legacy.join("notes.txt"), "mine").unwrap();
        assert_eq!(default_among(&files, &legacy, &snapshot), legacy);

        // Partway, the legacy folder is still complete and still chosen.
        publish(&snapshot, &text, &|_, partial| link_or_copy(&legacy.join(text.name), partial)).unwrap();
        assert_eq!(default_among(&files, &legacy, &snapshot), legacy);
        assert!(legacy.join(text.name).exists());

        adopt(&files, &legacy, &snapshot).unwrap();
        assert_eq!(default_among(&files, &legacy, &snapshot), snapshot);
        assert_eq!(fs::read(snapshot.join(text.name)).unwrap(), b"text");
        assert_eq!(fs::read(snapshot.join(media.name)).unwrap(), b"media");
        assert_eq!(snapshot.join(media.name).metadata().unwrap().permissions().mode() & 0o777, 0o644);
        assert!(!legacy.join(text.name).exists() && !legacy.join(media.name).exists());
        assert!(!legacy.join(".download.lock").exists() && !legacy.join("media.gguf.part").exists());
        assert_eq!(fs::read(legacy.join("notes.txt")).unwrap(), b"mine", "what isn't ours stays");

        // Running it again changes nothing, and an emptied folder goes.
        adopt(&files, &legacy, &snapshot).unwrap();
        fs::remove_file(legacy.join("notes.txt")).unwrap();
        adopt(&files, &legacy, &snapshot).unwrap();
        assert!(!legacy.exists());
    }

    #[test]
    fn a_legacy_file_already_in_the_cache_is_just_removed() {
        let base = tempfile::tempdir().unwrap();
        let legacy = base.path().join("Models");
        let snapshot = cached_snapshot(&base.path().join("hub"));
        let file = spec("text.gguf", b"text");
        fs::create_dir_all(&legacy).unwrap();
        install_with(&snapshot, &file, &|_, partial| fs::write(partial, b"text")).unwrap();
        // 0.6 hard-linked cached files into its folder.
        fs::hard_link(snapshot.join(file.name).canonicalize().unwrap(), legacy.join(file.name)).unwrap();
        adopt(&[file], &legacy, &snapshot).unwrap();
        assert!(!legacy.exists());
        assert_eq!(fs::read(snapshot.join(file.name)).unwrap(), b"text");
    }

    #[test]
    fn a_wrong_legacy_file_is_set_aside_and_not_adopted() {
        let base = tempfile::tempdir().unwrap();
        let legacy = base.path().join("Models");
        let snapshot = cached_snapshot(&base.path().join("hub"));
        let good = spec("good.gguf", b"good");
        let bad = spec("bad.gguf", b"same");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join(good.name), b"good").unwrap();
        fs::write(legacy.join(bad.name), b"evil").unwrap();
        assert_eq!(default_among(&[good, bad], &legacy, &snapshot), legacy);

        adopt(&[good, bad], &legacy, &snapshot).unwrap();
        assert_eq!(fs::read(snapshot.join(good.name)).unwrap(), b"good");
        assert!(!snapshot.join(bad.name).exists());
        assert_eq!(fs::read(legacy.join("bad.gguf.invalid")).unwrap(), b"evil");
        assert_eq!(default_among(&[good, bad], &legacy, &snapshot), snapshot);
    }

    #[test]
    fn a_failed_adoption_keeps_the_legacy_folder() {
        let base = tempfile::tempdir().unwrap();
        let legacy = base.path().join("Models");
        let hub = base.path().join("hub");
        let snapshot = cached_snapshot(&hub);
        let file = spec("text.gguf", b"text");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join(file.name), b"text").unwrap();
        fs::write(&hub, "a file where the cache should go").unwrap();
        assert!(adopt(&[file], &legacy, &snapshot).is_err());
        assert_eq!(fs::read(legacy.join(file.name)).unwrap(), b"text");
        assert_eq!(default_among(&[file], &legacy, &snapshot), legacy);
    }

    #[test]
    fn a_plain_folder_keeps_plain_files() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("my-models");
        // No name of the real files, so a cache on this machine can't be picked up.
        let file = spec("custom.gguf", b"weights");
        install_with(&folder, &file, &|_, partial| fs::write(partial, b"weights")).unwrap();
        assert!(folder.join(file.name).symlink_metadata().unwrap().is_file());
        assert_eq!(fs::read(folder.join(file.name)).unwrap(), b"weights");
        assert!(!folder.join("custom.gguf.part").exists());
        install_with(&folder, &file, &fails).unwrap();
        assert!(install_with(&folder, &spec("other.gguf", b"other"), &|_, partial| fs::write(partial, b"bad")).is_err());
        assert!(!folder.join("other.gguf").exists() && !folder.join("other.gguf.part").exists());
    }

    #[test]
    fn progress_counts_the_partial_file() {
        let hub = tempfile::tempdir().unwrap();
        let snapshot = cached_snapshot(hub.path());
        let blobs = hub.path().join(repo_folder()).join("blobs");
        fs::create_dir_all(&blobs).unwrap();
        fs::write(blobs.join(format!("{}.incomplete", TEXT.sha256)), b"12345").unwrap();
        assert_eq!(received(&snapshot, &TEXT), 5);
        assert_eq!(received(&snapshot, &MEDIA), 0);
        let flat = tempfile::tempdir().unwrap();
        fs::write(flat.path().join(format!("{}.part", TEXT.name)), b"123").unwrap();
        assert_eq!(received(flat.path(), &TEXT), 3);
    }
}
