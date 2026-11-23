//! Linux inotify with debounced sync and periodic recovery for overflow/watch exhaustion. The daemon sleeps in one
//! `poll` over the descriptors here until an event or the nearest deadline; nothing in this file wakes on a timer of
//! its own.
use super::{folders, Value};
use std::collections::HashSet;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
/// A burst of changes becomes one sync once the folder has been quiet for this long.
const QUIET: Duration = Duration::from_millis(750);
/// A folder that never goes quiet (a download, a build) is still synced this long after its first change.
const BURST: Duration = Duration::from_secs(5);
/// The safety net: inotify drops events when its queue overflows or the watch limit is reached, so the folders are
/// crawled again this often whatever was heard.
pub(super) const RECOVERY: Duration = Duration::from_secs(300);
const NOISE: &[&str] = &[
    "node_modules",
    "bower_components",
    "venv",
    "site-packages",
    "__pycache__",
    "DerivedData",
    "Pods",
    "Carthage",
    "vendor",
    "build",
    "dist",
    "target",
    "out",
    "Vendor",
    "third_party",
];
pub(super) struct Watcher {
    fd: Option<OwnedFd>,
    watched: HashSet<(std::path::PathBuf, bool)>,
    changed: Option<Instant>,
    first: Option<Instant>,
    recovery: Instant,
}
impl Watcher {
    pub fn new(config: &Value) -> Self {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        let mut result = Self {
            fd: if fd < 0 {
                None
            } else {
                Some(unsafe { OwnedFd::from_raw_fd(fd) })
            },
            watched: HashSet::new(),
            changed: None,
            first: None,
            recovery: Instant::now(),
        };
        result.add_roots(config);
        result
    }
    fn add_roots(&mut self, config: &Value) {
        for (key, excluded_key, code) in [
            ("roots", "excluded_folders", false),
            ("code_roots", "code_excluded", true),
        ] {
            let mut excluded = folders(config, excluded_key);
            for key in ["index_directory", "models_directory"] {
                if let Some(path) = config[key].as_str() {
                    excluded.push(path.to_string());
                }
            }
            for root in folders(config, key) {
                self.add(Path::new(&root), &excluded, code);
            }
        }
    }
    fn add(&mut self, path: &Path, excluded: &[String], code: bool) {
        if self.watched.contains(&(path.to_path_buf(), code))
            || excluded.iter().any(|p| path.starts_with(p))
        {
            return;
        }
        let Some(fd) = &self.fd else { return };
        use std::os::unix::ffi::OsStrExt;
        let Ok(name) = CString::new(path.as_os_str().as_bytes()) else {
            return;
        };
        let mask = libc::IN_CLOSE_WRITE
            | libc::IN_ATTRIB
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_DELETE_SELF
            | libc::IN_MOVE_SELF
            | libc::IN_ONLYDIR;
        if unsafe { libc::inotify_add_watch(fd.as_raw_fd(), name.as_ptr(), mask) } < 0 {
            return;
        }
        self.watched.insert((path.to_path_buf(), code));
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with('.')
                    && !NOISE.contains(&name.as_ref())
                    && !name.ends_with(".noindex")
                    && entry.file_type().is_ok_and(|t| t.is_dir())
                {
                    self.add(&entry.path(), excluded, code);
                }
            }
        }
    }
    /// The inotify descriptor, readable when a watched folder changed.
    pub fn fd(&self) -> Option<RawFd> {
        self.fd.as_ref().map(AsRawFd::as_raw_fd)
    }
    /// When `tick` next has something to say without a new event: the end of a pending debounce, else the recovery
    /// rescan. The daemon sleeps until then.
    pub fn next_deadline(&self) -> Instant {
        let mut deadline = self.recovery + RECOVERY;
        if let Some(changed) = self.changed {
            deadline = deadline.min(changed + QUIET);
        }
        if let Some(first) = self.first {
            deadline = deadline.min(first + BURST);
        }
        deadline
    }
    pub fn tick(&mut self, config: &Value) -> bool {
        let mut buffer = [0u8; 65536];
        let mut activity = false;
        let mut rebuild = false;
        if let Some(fd) = &self.fd {
            loop {
                let size =
                    unsafe { libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
                if size <= 0 {
                    break;
                }
                let mut offset = 0;
                while offset + 16 <= size as usize {
                    let mask =
                        u32::from_ne_bytes(buffer[offset + 4..offset + 8].try_into().unwrap());
                    let len =
                        u32::from_ne_bytes(buffer[offset + 12..offset + 16].try_into().unwrap())
                            as usize;
                    if offset + 16 + len > size as usize {
                        break;
                    }
                    let name = &buffer[offset + 16..offset + 16 + len];
                    if name.first() != Some(&b'.') {
                        activity = true;
                    }
                    rebuild |= mask
                        & (libc::IN_Q_OVERFLOW
                            | libc::IN_IGNORED
                            | libc::IN_DELETE_SELF
                            | libc::IN_MOVE_SELF)
                        != 0
                        || mask & libc::IN_ISDIR != 0;
                    offset += 16 + len;
                }
            }
        }
        if rebuild {
            // Rebuild after directory topology changes; event bursts still produce one sync.
            let first = self.first;
            let changed = self.changed;
            let recovery = self.recovery;
            *self = Self::new(config);
            self.first = first;
            self.changed = changed;
            self.recovery = recovery;
        }
        if activity {
            let now = Instant::now();
            self.changed = Some(now);
            self.first.get_or_insert(now);
        }
        let periodic = self.recovery.elapsed() >= RECOVERY;
        let settled = self.changed.is_some_and(|t| t.elapsed() >= QUIET)
            || self.first.is_some_and(|t| t.elapsed() >= BURST);
        if periodic || settled {
            self.changed = None;
            self.first = None;
            self.recovery = Instant::now();
            if periodic {
                *self = Self::new(config);
            }
            true
        } else {
            false
        }
    }
}

/// Watches one file that is replaced by renaming a new one over it (settings.json), by watching its folder for that
/// name. While the folder doesn't exist yet, the nearest existing ancestor is watched instead and any event there is
/// taken as a possible change, so a first `recall setup` after the daemon started is noticed too.
pub(super) struct FileWatch {
    path: PathBuf,
    fd: Option<OwnedFd>,
    /// False while an ancestor stands in for the missing folder.
    exact: bool,
}
impl FileWatch {
    pub fn new(path: &Path) -> Self {
        let mut result = Self { path: path.to_path_buf(), fd: None, exact: false };
        result.arm();
        result
    }
    /// A fresh descriptor on the file's folder, or on the nearest ancestor that exists. Starting over drops the old
    /// watch with its descriptor, which is what a replaced or deleted folder needs.
    fn arm(&mut self) {
        use std::os::unix::ffi::OsStrExt;
        self.fd = None;
        self.exact = false;
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mask = libc::IN_CLOSE_WRITE
            | libc::IN_MOVED_TO
            | libc::IN_MOVED_FROM
            | libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_DELETE_SELF
            | libc::IN_MOVE_SELF
            | libc::IN_ONLYDIR;
        let Some(parent) = self.path.parent() else { return };
        for folder in parent.ancestors() {
            let Ok(name) = CString::new(folder.as_os_str().as_bytes()) else { return };
            if unsafe { libc::inotify_add_watch(fd.as_raw_fd(), name.as_ptr(), mask) } >= 0 {
                self.exact = folder == parent;
                self.fd = Some(fd);
                return;
            }
        }
    }
    pub fn fd(&self) -> Option<RawFd> {
        self.fd.as_ref().map(AsRawFd::as_raw_fd)
    }
    /// Read what arrived; true when the file may have changed. The watch is renewed before this returns true for a
    /// folder that appeared, moved or vanished, so a caller that reads the file afterwards misses nothing: a write
    /// before the new watch is in what it reads, a write after it is the next event.
    pub fn changed(&mut self) -> bool {
        let Some(fd) = self.fd() else { return false };
        let name = self.path.file_name().map(|name| {
            use std::os::unix::ffi::OsStrExt;
            name.as_bytes().to_vec()
        });
        let mut buffer = [0u8; 4096];
        let mut changed = false;
        let mut renew = false;
        loop {
            let size = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if size <= 0 {
                break;
            }
            let mut offset = 0;
            while offset + 16 <= size as usize {
                let mask = u32::from_ne_bytes(buffer[offset + 4..offset + 8].try_into().unwrap());
                let len = u32::from_ne_bytes(buffer[offset + 12..offset + 16].try_into().unwrap()) as usize;
                if offset + 16 + len > size as usize {
                    break;
                }
                // The kernel pads the name with NULs to align the next event.
                let raw = &buffer[offset + 16..offset + 16 + len];
                let event_name = raw.split(|&b| b == 0).next().unwrap_or(&[]);
                if mask & (libc::IN_Q_OVERFLOW | libc::IN_IGNORED | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0 {
                    renew = true;
                } else if !self.exact {
                    // Something appeared under an ancestor: perhaps the folder being waited for.
                    renew = true;
                } else if name.as_deref() == Some(event_name) {
                    changed = true;
                }
                offset += 16 + len;
            }
        }
        if renew {
            self.arm();
        }
        changed || renew
    }
}

/// Sleep until one of the descriptors is readable or the deadline passes, whichever is first. This is the daemon's
/// only wait: with nothing pending the deadline is minutes away.
pub(super) fn wait(fds: &[Option<RawFd>], deadline: Instant) {
    let mut polled: Vec<libc::pollfd> =
        fds.iter().flatten().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    // Rounded up, so a wake at the deadline finds it passed instead of spinning through the last millisecond.
    let timeout = deadline.saturating_duration_since(Instant::now()).as_millis().saturating_add(1).min(i32::MAX as u128);
    unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, timeout as i32) };
}

#[cfg(test)]
mod tests {
    use super::*;
    fn readable(fd: Option<RawFd>) -> bool {
        let mut polled = [libc::pollfd { fd: fd.unwrap(), events: libc::POLLIN, revents: 0 }];
        unsafe { libc::poll(polled.as_mut_ptr(), 1, 2000) > 0 }
    }
    #[test]
    fn sleeps_until_the_debounce_ends_and_otherwise_until_the_recovery_rescan() {
        let directory = tempfile::tempdir().unwrap();
        let config = serde_json::json!({"roots":[directory.path()],"code_roots":[],"excluded_folders":[],"code_excluded":[]});
        let mut watcher = Watcher::new(&config);
        // Idle, the only deadline is the safety rescan, minutes away.
        assert!(watcher.next_deadline() > Instant::now() + RECOVERY - Duration::from_secs(5));
        std::fs::write(directory.path().join("file"), "one").unwrap();
        // The change itself wakes the sleeper, with no timer involved.
        assert!(readable(watcher.fd()));
        let started = Instant::now();
        wait(&[watcher.fd()], started + Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!watcher.tick(&config));
        // Pending, the deadline is the end of the quiet period, and sleeping until then is enough for the sync.
        let deadline = watcher.next_deadline();
        assert!(deadline <= Instant::now() + QUIET);
        wait(&[watcher.fd()], deadline);
        assert!(watcher.tick(&config));
        assert!(watcher.next_deadline() > Instant::now() + RECOVERY - Duration::from_secs(5));
    }
    #[test]
    fn waiting_ends_at_the_deadline_when_nothing_happens() {
        let started = Instant::now();
        wait(&[None], started + Duration::from_millis(50));
        assert!(started.elapsed() >= Duration::from_millis(50));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn a_replaced_settings_file_is_noticed_and_its_neighbours_are_not() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut watch = FileWatch::new(&path);
        assert!(!watch.changed());
        // The way settings are saved: a temporary file renamed over the real one.
        let temporary = directory.path().join("settings.tmp");
        std::fs::write(&temporary, "{}").unwrap();
        assert!(readable(watch.fd()));
        assert!(!watch.changed(), "a temporary file is not the settings");
        std::fs::rename(&temporary, &path).unwrap();
        assert!(readable(watch.fd()));
        assert!(watch.changed());
        assert!(!watch.changed());
        std::fs::write(&path, "{\"roots\":[]}").unwrap();
        assert!(readable(watch.fd()));
        assert!(watch.changed());
        std::fs::remove_file(&path).unwrap();
        assert!(readable(watch.fd()));
        assert!(watch.changed());
    }
    #[test]
    fn a_settings_folder_made_later_or_replaced_is_followed() {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join("config/recall");
        let path = folder.join("settings.json");
        let mut watch = FileWatch::new(&path);
        assert!(!watch.exact);
        std::fs::create_dir_all(&folder).unwrap();
        assert!(readable(watch.fd()));
        assert!(watch.changed());
        // Each new level renews the watch one folder nearer.
        while !watch.exact {
            assert!(readable(watch.fd()));
            assert!(watch.changed());
        }
        std::fs::write(&path, "{}").unwrap();
        assert!(readable(watch.fd()));
        assert!(watch.changed());
        // The folder is swapped for another: the old watch dies with it and the new folder is watched.
        std::fs::rename(&folder, directory.path().join("config/old")).unwrap();
        assert!(readable(watch.fd()));
        assert!(watch.changed());
        std::fs::create_dir_all(&folder).unwrap();
        while !watch.exact {
            assert!(readable(watch.fd()));
            assert!(watch.changed());
        }
        std::fs::write(&path, "{}").unwrap();
        assert!(readable(watch.fd()));
        assert!(watch.changed());
    }
    #[test]
    fn observes_new_and_nested_files_after_debounce() {
        let directory = tempfile::tempdir().unwrap();
        let config = serde_json::json!({"roots":[directory.path()],"code_roots":[],"excluded_folders":[],"code_excluded":[]});
        let mut watcher = Watcher::new(&config);
        std::fs::write(directory.path().join("file"), "one").unwrap();
        assert!(!watcher.tick(&config));
        std::thread::sleep(Duration::from_millis(800));
        assert!(watcher.tick(&config));
        std::fs::create_dir(directory.path().join("nested")).unwrap();
        watcher.tick(&config);
        std::fs::write(directory.path().join("nested/file"), "two").unwrap();
        watcher.tick(&config);
        std::thread::sleep(Duration::from_millis(800));
        assert!(watcher.tick(&config));
    }
}
