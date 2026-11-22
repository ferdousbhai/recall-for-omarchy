use super::*;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::Instant;

struct Rpc {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    reloaded: Instant,
}
impl Rpc {
    fn send(&mut self, value: &Value) -> Result<Value> {
        writeln!(self.input, "{value}")?;
        self.input.flush()?;
        let mut ready = libc::pollfd {
            fd: self.output.get_ref().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if self.output.buffer().is_empty() && unsafe { libc::poll(&mut ready, 1, 110_000) } <= 0 {
            bail!("Search worker timed out");
        }
        read_json(&mut self.output, 10 * LIMIT)
    }
}
impl Drop for Rpc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct Helper {
    child: Child,
    input: ChildStdin,
    stopping: bool,
}
impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
type RpcMessage = (Value, mpsc::Sender<Result<Value>>);

struct State {
    rpc_tx: mpsc::SyncSender<RpcMessage>,
    shared: super::shared::Service,
    model_requests: AtomicUsize,
    models_prepared: AtomicBool,
    config: Mutex<Value>,
    status: Mutex<Value>,
    rpc: Mutex<Option<Rpc>>,
    helper: Mutex<Option<Helper>>,
    dirty: AtomicBool,
    sync: AtomicBool,
    paused: AtomicBool,
    connections: AtomicUsize,
    /// Ends the background loop's sleep when another thread has work for it.
    wake: Wake,
    /// Set when the settings may read differently although the file did not change (the default model folder moved).
    reload: AtomicBool,
    /// Ends the model thread's wait: the settings changed, a caller needs a model, or someone asked for the status.
    models_wake: (Mutex<bool>, Condvar),
}
/// An eventfd the background loop polls with its other descriptors. Flags alone would only be seen at the next
/// timeout, and that is minutes away when nothing else happens.
struct Wake(Option<OwnedFd>);
impl Wake {
    fn new() -> Self {
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        Self((fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) }))
    }
    fn fd(&self) -> Option<RawFd> {
        self.0.as_ref().map(AsRawFd::as_raw_fd)
    }
    fn notify(&self) {
        if let Some(fd) = self.fd() {
            let one = 1u64;
            unsafe { libc::write(fd, (&one as *const u64).cast(), 8) };
        }
    }
    /// Reading resets the counter, so one wake answers any number of notifications.
    fn drain(&self) {
        if let Some(fd) = self.fd() {
            let mut count = 0u64;
            unsafe { libc::read(fd, (&mut count as *mut u64).cast(), 8) };
        }
    }
}
/// How long the loop sleeps without an eventfd (the kernel refused one): flags are then found by looking.
const WITHOUT_WAKE: Duration = Duration::from_secs(1);
fn launch(config: &Value, command: &str) -> Result<Command> {
    let mut cmd = Command::new(engine()?);
    cmd.env("RECALL_SHARED_SOCKET", socket_path());
    cmd.arg(command)
        .args(["--index", config["index_directory"].as_str().unwrap()])
        .env(
            "RECALL_MODELS",
            config["models_directory"].as_str().unwrap(),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let log = Path::new(config["index_directory"].as_str().unwrap()).join(if command == "serve" {
        "serve.log"
    } else {
        "indexer.log"
    });
    cmd.stderr(
        OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(log)?,
    );
    let parent = unsafe { libc::getpid() };
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(io::Error::other("Parent exited"));
            }
            Ok(())
        });
    }
    Ok(cmd)
}
impl State {
    /// Ask the background loop to sync the folders now.
    fn request_sync(&self) {
        self.sync.store(true, Ordering::Release);
        self.wake.notify();
    }
    /// Ask the background loop to read the settings again now.
    fn request_reload(&self) {
        self.reload.store(true, Ordering::Release);
        self.wake.notify();
    }
    fn wake_models(&self) {
        *self.models_wake.0.lock().unwrap() = true;
        self.models_wake.1.notify_all();
    }
    /// The model thread's wait between looks at the model folder: until it is woken, or the timeout as a safety net
    /// for files that appear or vanish without anyone telling it.
    fn wait_models(&self, timeout: Duration) {
        let (lock, wake) = &self.models_wake;
        let (mut woken, _) = wake.wait_timeout_while(lock.lock().unwrap(), timeout, |woken| !*woken).unwrap();
        *woken = false;
    }
    fn engine_request(&self, value: &Value) -> Result<Value> {
        let (tx, rx) = mpsc::channel();
        self.rpc_tx
            .try_send((value.clone(), tx))
            .context("Search coordinator stopped")?;
        rx.recv_timeout(Duration::from_secs(115))
            .context("Search request timed out")?
    }
    fn engine_request_inner(&self, value: &Value) -> Result<Value> {
        let config = self.config.lock().unwrap().clone();
        let mut rpc = self.rpc.lock().unwrap();
        if rpc
            .as_mut()
            .is_some_and(|r| r.child.try_wait().ok().flatten().is_some())
        {
            *rpc = None;
        }
        if rpc.is_none() && value["op"] == "release" {
            return Ok(json!({"ok":true}));
        }
        if rpc.is_none() {
            let mut child = launch(&config, "serve")?
                .args(["--idle", &config["encoder_idle_seconds"].to_string()])
                .spawn()?;
            let mut process = Rpc {
                input: child.stdin.take().unwrap(),
                output: BufReader::new(child.stdout.take().unwrap()),
                reloaded: Instant::now() - Duration::from_secs(2),
                child,
            };
            process.send(&json!({"op":"configure","roots":config["roots"],"skipped":config["excluded_folders"],
                "types":config["excluded_types"],"code_roots":config["code_roots"],"code_skipped":config["code_excluded"]}))?;
            *rpc = Some(process);
        }
        if rpc.as_ref().unwrap().reloaded.elapsed() >= Duration::from_secs(2)
            && self.dirty.swap(false, Ordering::AcqRel)
        {
            rpc.as_mut().unwrap().send(&json!({"op":"reload"}))?;
            rpc.as_mut().unwrap().reloaded = Instant::now();
        }
        let result = rpc.as_mut().unwrap().send(value);
        if result.is_err() {
            *rpc = None;
        }
        result
    }
    fn dispatch(&self, value: &Value) -> Result<Value> {
        let op = value["op"].as_str().context("Missing operation")?;
        match op {
            "capabilities" => Ok(
                json!({"api":1,"operations":["embed","records.search","records.forget"],"source":recall::llama::model_info().vector_source(),"model_repository":model_files::REPOSITORY,"model_revision":model_files::REVISION,"models_directory":self.config.lock().unwrap()["models_directory"],"batch_size":32}),
            ),
            "embed" | "records.search" | "records.forget" => {
                let config = self.config.lock().unwrap().clone();
                let result = self.shared.request(value, config);
                if result
                    .as_ref()
                    .is_err_and(|e| e.to_string().starts_with("Recall models are not ready"))
                {
                    let media = value["kind"] == "image"
                        || value["kind"] == "audio"
                        || value["records"].as_array().is_some_and(|r| {
                            r.iter()
                                .any(|v| v["kind"] == "image" || v["kind"] == "audio")
                        });
                    self.model_requests
                        .fetch_or(if media { 3 } else { 1 }, Ordering::Release);
                    self.wake_models();
                }
                result
            }

            "status" => {
                // Someone is looking, so the download state is brought up to date for the next look; nothing checks
                // the model folder on a timer otherwise.
                self.wake_models();
                let mut s = self.status.lock().unwrap().clone();
                let config = self.config.lock().unwrap();
                s["roots"] = config["roots"].clone();
                s["code_roots"] = config["code_roots"].clone();
                s["paused"] = self.paused.load(Ordering::Acquire).into();
                if folders(&config, "roots").is_empty() && folders(&config, "code_roots").is_empty()
                {
                    s["line"] = json!("Choose folders in Settings to start indexing");
                }
                if s["download"] == "downloading" {
                    let folder = Path::new(config["models_directory"].as_str().unwrap());
                    let received: u64 = model_files::ALL
                        .iter()
                        .map(|file| model_files::received(folder, file))
                        .sum();
                    let total: u64 = model_files::ALL.iter().map(|f| f.bytes).sum();
                    s["download_bytes"] = received.into();
                    s["download_total"] = total.into();
                    s["line"] = json!(format!(
                        "Downloading model · {:.0}%",
                        received as f64 / total as f64 * 100.0
                    ));
                }
                Ok(s)
            }
            "config" => Ok(json!({"config":self.config.lock().unwrap().clone()})),
            "configure" => {
                let updates = value["config"]
                    .as_object()
                    .context("Missing configuration")?;
                let mut config = self.config.lock().unwrap().clone();
                for (k, v) in updates {
                    if !defaults().as_object().unwrap().contains_key(k) {
                        bail!("Unknown setting: {k}");
                    }
                    config[k] = v.clone();
                }
                settle_models(&mut config);
                // Reject invalid values here instead of saving a file the daemon would then refuse to load.
                validate_config(&config)?;
                save_config(&config)?;
                Ok(json!({"ok":true,"config":config}))
            }
            "pause" => {
                let paused = value["paused"]
                    .as_bool()
                    .context("paused must be boolean")?;
                self.paused.store(paused, Ordering::Release);
                if paused {
                    if let Some(h) = self.helper.lock().unwrap().as_mut() {
                        // An indexer that already finished and exited by itself has nothing to stop, and its closed
                        // pipe must not turn the pause into an error.
                        let _ = writeln!(h.input, "stop");
                        h.stopping = true;
                    }
                } else {
                    self.request_sync();
                }
                Ok(json!({"ok":true}))
            }
            "search" => {
                let query = value["query"].as_str().context("Missing query")?;
                if query.is_empty() || query.chars().count() > 4096 {
                    bail!("Query must contain 1–4096 characters");
                }
                let limit = value
                    .get("limit")
                    .map(|v| v.as_u64().context("Invalid limit"))
                    .transpose()?
                    .unwrap_or(30);
                if !(1..=100).contains(&limit) {
                    bail!("Invalid limit");
                }
                if let Some(kinds) = value.get("kinds") {
                    if query.starts_with("code:")
                        || !kinds.as_array().is_some_and(|a| {
                            a.iter().all(|k| {
                                k.as_str().is_some_and(|s| {
                                    ["image", "screenshot", "pdf", "doc", "audio", "video"]
                                        .contains(&s)
                                })
                            })
                        })
                    {
                        bail!("Invalid kinds or code/kind combination");
                    }
                }
                let config = self.config.lock().unwrap().clone();
                let code = query.starts_with("code:");
                if folders(&config, if code { "code_roots" } else { "roots" }).is_empty() {
                    return Ok(json!({"hits":[],"strong":0,"meaning":false,"code":code}));
                }
                let mut reply = self.engine_request(value)?;
                let code = reply["code"].as_bool().unwrap_or(false);
                let config = self.config.lock().unwrap().clone();
                if let Some(hits) = reply["hits"].as_array_mut() {
                    hits.retain(|hit| {
                        hit["path"]
                            .as_str()
                            .is_some_and(|path| in_scope(Path::new(path), &config, code))
                    });
                    for hit in hits {
                        let path = Path::new(hit["path"].as_str().unwrap_or(""));
                        let name = path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string();
                        let folder = path
                            .parent()
                            .unwrap_or(Path::new(""))
                            .to_string_lossy()
                            .to_string();
                        let loc = hit["loc"].as_f64().unwrap_or(0.0);
                        let end = hit["loc_end"].as_f64().unwrap_or(loc);
                        let location = match hit["segment"].as_str().unwrap_or("") {
                            "page" | "page_image" => format!("p. {}", loc as u64),
                            "audio" | "frame" => {
                                format!("{}:{:02}", loc as u64 / 60, loc as u64 % 60)
                            }
                            "lines" => format!("L{}–{}", loc as u64, end as u64),
                            "chunk" => format!("part {}", loc as u64),
                            "cells" => format!("cell {}", loc as u64),
                            _ => String::new(),
                        };
                        hit["name"] = name.into();
                        hit["folder"] = folder.into();
                        hit["location"] = location.into();
                        hit["code"] = code.into();
                    }
                }
                Ok(reply)
            }
            "text" => {
                if value["segment"].as_u64().unwrap_or(0) == 0 {
                    bail!("Segment must be positive");
                }
                let code = value["code"].as_bool().unwrap_or(false);
                let config = self.config.lock().unwrap().clone();
                if folders(&config, if code { "code_roots" } else { "roots" }).is_empty() {
                    return Ok(json!({"text":null}));
                }
                let mut reply = self.engine_request(value)?;
                let config = self.config.lock().unwrap().clone();
                if !reply["path"]
                    .as_str()
                    .is_some_and(|path| in_scope(Path::new(path), &config, code))
                {
                    reply["text"] = Value::Null;
                }
                if let Some(object) = reply.as_object_mut() {
                    object.remove("path");
                }
                Ok(reply)
            }
            "prewarm" => {
                let config = self.config.lock().unwrap();
                if folders(&config, "roots").is_empty() && folders(&config, "code_roots").is_empty()
                {
                    return Ok(json!({"ok":true}));
                }
                drop(config);
                self.engine_request(value)
            }
            "release" => self.engine_request(value),
            "open" | "reveal" => {
                open_hit(&value["hit"], &self.config.lock().unwrap(), op == "reveal")?;
                Ok(json!({"ok":true}))
            }
            "settings" => {
                toggle(true)?;
                Ok(json!({"ok":true}))
            }
            _ => bail!("Unknown operation: {op}"),
        }
    }
    /// Start the indexer or tell the running one to sync. False when a paused indexer is still shutting down: there
    /// is no event for its exit, so the caller tries again shortly.
    fn start_helper(self: &Arc<Self>, backfill: bool) -> Result<bool> {
        // Nothing is embedded with model files that haven't been checked; preparing asks for a sync when it's done.
        if !self.models_prepared.load(Ordering::Acquire) {
            return Ok(true);
        }
        let config = self.config.lock().unwrap().clone();
        if self.paused.load(Ordering::Acquire)
            || (folders(&config, "roots").is_empty() && folders(&config, "code_roots").is_empty())
        {
            return Ok(true);
        }
        let mut holder = self.helper.lock().unwrap();
        if let Some(h) = holder.as_mut() {
            if h.child.try_wait()?.is_none() {
                if h.stopping {
                    return Ok(false);
                }
                writeln!(h.input, "sync")?;
                writeln!(h.input, "sync code")?;
                return Ok(true);
            }
        }
        *holder = None;
        let mut cmd = launch(&config, "index-service")?;
        cmd.arg("--prune");
        if backfill {
            cmd.arg("--backfill");
        }
        for (key, flag) in [
            ("excluded_folders", "--exclude"),
            ("excluded_types", "--exclude-ext"),
            ("code_roots", "--code-root"),
            ("code_excluded", "--code-exclude"),
        ] {
            for path in folders(&config, key) {
                cmd.args([flag, &path]);
            }
        }
        cmd.args(folders(&config, "roots"));
        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().unwrap();
        let input = child.stdin.take().unwrap();
        *holder = Some(Helper {
            child,
            input,
            stopping: false,
        });
        let state = Arc::clone(self);
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let Ok(event) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let mut status = state.status.lock().unwrap();
                for (key, value) in event.as_object().into_iter().flatten() {
                    if key != "id" {
                        status[key] = value.clone();
                    }
                }
                let kind = event["event"].as_str().unwrap_or("");
                if ["file", "summary", "idle", "pruned", "rest"].contains(&kind) {
                    state.dirty.store(true, Ordering::Release);
                }
                status["line"] = json!(match kind {
                    "file" | "rest" => format!("Indexing · {} / {}", event["done"], event["total"]),
                    "sync" => "Checking folders…".into(),
                    "waiting" => "Waiting for model download…".into(),
                    "error" => format!("Indexing: {}", event["message"]),
                    _ => format!(
                        "{} files indexed",
                        status["searchable"].as_u64().unwrap_or(0)
                    ),
                });
                status["activity"] = json!(if kind == "idle" {
                    "idle"
                } else if kind == "sync" {
                    "syncing"
                } else if kind == "file" || kind == "rest" {
                    "indexing"
                } else {
                    kind
                });
            }
        });
        Ok(true)
    }
}
/// The power-profiles daemon under its current name and the one it had before.
const PROFILE_SERVICES: [(&str, &str); 2] = [
    ("org.freedesktop.UPower.PowerProfiles", "/org/freedesktop/UPower/PowerProfiles"),
    ("net.hadess.PowerProfiles", "/net/hadess/PowerProfiles"),
];
/// The first profile service that answers, and whether it is saving power.
fn power_profile() -> Option<(usize, bool)> {
    PROFILE_SERVICES.iter().enumerate().find_map(|(index, (service, path))| {
        Command::new("busctl")
            .args(["--system", "--timeout=1s", "get-property", service, path, service, "ActiveProfile"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| (index, String::from_utf8_lossy(&output.stdout).contains("\"power-saver\"")))
    })
}
fn backfill_allowed(config: &Value) -> bool {
    let battery = fs::read_dir("/sys/class/power_supply")
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            fs::read_to_string(e.path().join("status")).is_ok_and(|s| s.trim() == "Discharging")
        });
    if battery && config["index_on_battery"] == false {
        return false;
    }
    !power_profile().is_some_and(|(_, saver)| saver)
}
/// True for a kernel uevent about a power supply (a charger plugged in or pulled, a battery changing state). The
/// message is the action line followed by NUL-separated KEY=value fields.
fn power_supply_event(message: &[u8]) -> bool {
    message.split(|&b| b == 0).any(|field| field == b"SUBSYSTEM=power_supply")
}
/// What tells the background loop that the power policy may have changed, so it need not look on a timer: the
/// kernel's uevents for the battery and charger, and the profile daemon's D-Bus signals for power-saver.
struct PowerEvents {
    /// A NETLINK_KOBJECT_UEVENT socket. It needs no privilege, but a sandbox may refuse it.
    uevents: Option<OwnedFd>,
    /// `gdbus monitor` on the profile service; each signal is a line on its output.
    profile: Option<Child>,
}
impl PowerEvents {
    /// The safety re-check with both event sources, in case an event was lost (the uevent queue can overflow).
    const SAFETY: Duration = Duration::from_secs(300);
    /// The re-check when a source is missing. Still far from the 10 seconds this used to be: the policy only decides
    /// whether a long backfill may run.
    const WITHOUT_EVENTS: Duration = Duration::from_secs(60);
    fn new() -> Self {
        Self { uevents: Self::uevent_socket(), profile: Self::profile_monitor() }
    }
    fn uevent_socket() -> Option<OwnedFd> {
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                libc::NETLINK_KOBJECT_UEVENT,
            )
        };
        if fd < 0 {
            return None;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        address.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        // Group 1 is the kernel's own broadcast; udev's rebroadcast (group 2) would say the same again.
        address.nl_groups = 1;
        let bound = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&address as *const libc::sockaddr_nl).cast(),
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        (bound == 0).then_some(fd)
    }
    fn profile_monitor() -> Option<Child> {
        let (service, path) = PROFILE_SERVICES[power_profile()?.0];
        let mut command = Command::new("gdbus");
        command
            .args(["monitor", "--system", "--dest", service, "--object-path", path])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // The loop's thread lives as long as the daemon, so the parent-death signal means the daemon died.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().ok()?;
        let fd = child.stdout.as_ref()?.as_raw_fd();
        // Drained without blocking, after poll says there is something.
        unsafe { libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK) };
        Some(child)
    }
    fn fds(&self) -> [Option<RawFd>; 2] {
        [
            self.uevents.as_ref().map(AsRawFd::as_raw_fd),
            self.profile.as_ref().and_then(|child| child.stdout.as_ref()).map(AsRawFd::as_raw_fd),
        ]
    }
    fn interval(&self) -> Duration {
        if self.uevents.is_some() && self.profile.is_some() {
            Self::SAFETY
        } else {
            Self::WITHOUT_EVENTS
        }
    }
    /// Read what arrived; true when the power policy should be looked at again.
    fn changed(&mut self) -> bool {
        let mut changed = false;
        let mut buffer = [0u8; 8192];
        if let Some(fd) = &self.uevents {
            loop {
                let size = unsafe { libc::recv(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len(), 0) };
                if size > 0 {
                    // Every device's events arrive here (they are rare); only a power supply's matter.
                    changed |= power_supply_event(&buffer[..size as usize]);
                } else if size < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ENOBUFS) {
                    // The queue overflowed and dropped events, perhaps the one that mattered.
                    changed = true;
                } else {
                    break;
                }
            }
        }
        let mut ended = false;
        if let Some(fd) = self.fds()[1] {
            loop {
                let size = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
                if size > 0 {
                    changed = true;
                } else {
                    // End of file: the monitor exited (no system bus, or it was killed).
                    ended = size == 0;
                    break;
                }
            }
        }
        if ended {
            if let Some(mut child) = self.profile.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        changed
    }
}
impl Drop for PowerEvents {
    fn drop(&mut self) {
        if let Some(child) = &mut self.profile {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
/// Check the model folder whenever it changes (moving an earlier install's files in on the way), then download what's
/// wanted and missing. A check and a move need no network, so they also run offline.
fn maintain_models(state: Arc<State>, offline: bool) {
    thread::spawn(move || {
        let mut prepared = None;
        loop {
            let config = state.config.lock().unwrap().clone();
            let saved = config["models_directory"].as_str().unwrap();
            let folder = model_files::resolve_setting(saved);
            if prepared.as_ref() != Some(&folder) {
                if let Err(e) = model_files::prepare(&folder) {
                    eprintln!("Models: {e}");
                }
                // Moving the files moves the default folder without the settings file changing, so the background loop
                // is told to read the settings again; it tells the workers and wakes this thread with the result.
                prepared = Some(model_files::resolve_setting(saved));
                state.models_prepared.store(true, Ordering::Release);
                state.request_reload();
                state.request_sync();
                continue;
            }
            if offline {
                state.status.lock().unwrap()["download"] =
                    json!(if model_files::missing(&folder).is_empty() { "ready" } else { "offline" });
                state.wait_models(MODELS_SAFETY);
            } else {
                download_missing(&state, &config, &folder);
            }
        }
    });
}
/// How long the model thread waits when nothing tells it to look: new settings, a caller needing a model and a status
/// request all wake it at once, so this only covers model files changed by hand while nobody is looking.
const MODELS_SAFETY: Duration = Duration::from_secs(300);
fn download_missing(state: &State, config: &Value, folder: &Path) {
    let wanted = if !folders(config, "roots").is_empty()
        || !folders(config, "code_roots").is_empty()
    {
        3
    } else {
        state.model_requests.load(Ordering::Acquire)
    };
    let missing: Vec<_> = model_files::missing(folder)
        .into_iter()
        .filter(|f| {
            wanted
                & if f.name == model_files::TEXT.name {
                    1
                } else {
                    2
                }
                != 0
        })
        .collect();
    if wanted == 0 || missing.is_empty() {
        state.status.lock().unwrap()["download"] = json!(if wanted == 0 && !model_files::missing(folder).is_empty() {
                "needed"
            } else {
                "ready"
            });
        state.wait_models(MODELS_SAFETY);
        return;
    }
    state.status.lock().unwrap()["download"] = json!("downloading");
    let result = missing.iter().try_for_each(|file| model_files::install(folder, file));
    match result {
        Ok(()) => {
            state.status.lock().unwrap()["download"] = json!("ready");
            state.request_sync();
        }
        Err(e) => {
            let mut s = state.status.lock().unwrap();
            s["download"] = json!("retrying");
            s["line"] = json!(format!("Model download: {e}"));
        }
    }
    // A pause between attempts, so a failing download is not retried in a tight loop; nothing should cut it short.
    thread::sleep(Duration::from_secs(30));
}
pub(super) fn run() -> Result<i32> {
    // Acquire an exclusive lock before touching the socket; a second daemon cannot replace a live one.
    let socket = socket_path();
    let runtime = socket.parent().context("Missing runtime directory")?;
    let metadata = fs::metadata(runtime)?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
        bail!("Runtime directory must belong to this user and not be group/world writable");
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(socket.with_extension("lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("Recall daemon already running");
    }
    unsafe {
        libc::umask(0o077);
    }
    match fs::remove_file(&socket) {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let config = load_config()?;
    fs::create_dir_all(config["index_directory"].as_str().unwrap())?;
    let (rpc_tx, rpc_rx) = mpsc::sync_channel::<RpcMessage>(32);
    let state = Arc::new(State {
        rpc_tx,
        shared: super::shared::Service::start(),
        model_requests: AtomicUsize::new(0),
        models_prepared: AtomicBool::new(false),
        config: Mutex::new(config.clone()),
        status: Mutex::new(
            json!({"line":"Starting…","download":"needed","paused":false,"files":0,"searchable":0,"pending":0,"code_files":0,"code_pending":0}),
        ),
        rpc: Mutex::new(None),
        helper: Mutex::new(None),
        dirty: AtomicBool::new(false),
        sync: AtomicBool::new(true),
        paused: AtomicBool::new(false),
        connections: AtomicUsize::new(0),
        wake: Wake::new(),
        reload: AtomicBool::new(false),
        models_wake: (Mutex::new(false), Condvar::new()),
    });
    let owner = Arc::clone(&state);
    thread::spawn(move || {
        // This thread outlives individual socket connections. Linux PDEATHSIG is tied to
        // the spawning thread, so short-lived request threads must never launch workers.
        for (request, reply) in rpc_rx {
            let _ = reply.send(owner.engine_request_inner(&request));
        }
    });
    // After a package update the restarted service brings this user's panel and skill up to date.
    thread::spawn(|| {
        if let Err(e) = super::install::refresh() {
            eprintln!("Refreshing the panel and skill: {e}");
        }
    });
    maintain_models(Arc::clone(&state), std::env::var("RECALL_OFFLINE").as_deref() == Ok("1"));
    let background = Arc::clone(&state);
    thread::spawn(move || {
        let mut config = config;
        let mut watcher = watcher::Watcher::new(&config);
        let mut settings = watcher::FileWatch::new(&config_path());
        let mut power = PowerEvents::new();
        let mut policy = backfill_allowed(&config);
        let mut config_check = Instant::now();
        let mut power_check = Instant::now();
        let mut power_due = false;
        // When to ask the indexer again after it could not take a sync.
        let mut retry: Option<Instant> = None;
        // Each pass does what is due and then sleeps until an event or the nearest deadline. A request made while a
        // pass runs is not lost: its wake stays readable, so the sleep ends at once.
        loop {
            // The settings are read again when their file changes. The slow re-read is a safety net for a missed
            // event (a queue overflow, a filesystem without inotify), on the same schedule as the folder rescan.
            let reload = settings.changed() | background.reload.swap(false, Ordering::AcqRel);
            if reload || config_check.elapsed() >= watcher::RECOVERY {
                config_check = Instant::now();
                match load_config() {
                    Ok(new) if new != config => {
                        let mut helper = background.helper.lock().unwrap();
                        if let Some(h) = helper.as_mut() {
                            let _ = h.child.kill();
                            let _ = h.child.wait();
                        }
                        *helper = None;
                        drop(helper);
                        *background.rpc.lock().unwrap() = None;
                        // The battery setting is half of the power policy.
                        power_due |= new["index_on_battery"] != config["index_on_battery"];
                        config = new;
                        *background.config.lock().unwrap() = config.clone();
                        let _ = fs::create_dir_all(config["index_directory"].as_str().unwrap());
                        watcher = watcher::Watcher::new(&config);
                        background.sync.store(true, Ordering::Release);
                        // The model thread follows the folders and the model folder.
                        background.wake_models();
                    }
                    Err(e) => {
                        eprintln!("Settings: {e}");
                    }
                    _ => (),
                }
            }
            if power.changed() | std::mem::take(&mut power_due) || power_check.elapsed() >= power.interval() {
                power_check = Instant::now();
                let next = backfill_allowed(&config);
                if next != policy {
                    policy = next;
                    if let Some(h) = background.helper.lock().unwrap().as_mut() {
                        let _ = writeln!(h.input, "backfill {}", if policy { "on" } else { "off" });
                    }
                }
            }
            if watcher.tick(&config) {
                background.sync.store(true, Ordering::Release);
            }
            if retry.is_some_and(|at| at <= Instant::now()) {
                retry = None;
                background.sync.store(true, Ordering::Release);
            }
            if background.sync.swap(false, Ordering::AcqRel) {
                retry = match background.start_helper(policy) {
                    Ok(true) => None,
                    // A paused indexer is still exiting; it is gone within moments.
                    Ok(false) => Some(Instant::now() + Duration::from_millis(250)),
                    Err(e) => {
                        eprintln!("Indexer: {e}");
                        Some(Instant::now() + Duration::from_secs(1))
                    }
                };
            }
            let mut deadline = watcher
                .next_deadline()
                .min(config_check + watcher::RECOVERY)
                .min(power_check + power.interval());
            if let Some(at) = retry {
                deadline = deadline.min(at);
            }
            if background.wake.fd().is_none() {
                deadline = deadline.min(Instant::now() + WITHOUT_WAKE);
            }
            let [uevents, profile] = power.fds();
            watcher::wait(&[background.wake.fd(), watcher.fd(), settings.fd(), uevents, profile], deadline);
            background.wake.drain();
        }
    });
    for connection in listener.incoming() {
        let stream = connection?;
        if state.connections.fetch_add(1, Ordering::AcqRel) >= 32 {
            state.connections.fetch_sub(1, Ordering::Release);
            continue;
        }
        let state = Arc::clone(&state);
        thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            let mut reader = BufReader::new(stream);
            while let Ok(value) = read_json(&mut reader, LIMIT) {
                let mut reply = state
                    .dispatch(&value)
                    .unwrap_or_else(|e| json!({"error":e.to_string()}));
                if ["capabilities", "embed", "records.search", "records.forget"]
                    .contains(&value["op"].as_str().unwrap_or(""))
                {
                    reply["api"] = json!(1);
                }
                reply["id"] = value.get("id").cloned().unwrap_or(Value::Null);
                if writeln!(reader.get_mut(), "{reply}").is_err() {
                    break;
                }
            }
            state.connections.fetch_sub(1, Ordering::Release);
        });
    }
    drop(lock);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_wake_ends_the_sleep_at_once_and_is_spent_by_draining() {
        let wake = Wake::new();
        let started = Instant::now();
        watcher::wait(&[wake.fd()], started + Duration::from_millis(50));
        assert!(started.elapsed() >= Duration::from_millis(50), "nothing was asked, so it sleeps to the deadline");
        // Asked twice before the sleep, as two request threads might: one wake answers both.
        wake.notify();
        wake.notify();
        let started = Instant::now();
        watcher::wait(&[wake.fd()], started + Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_secs(5));
        wake.drain();
        let started = Instant::now();
        watcher::wait(&[wake.fd()], started + Duration::from_millis(50));
        assert!(started.elapsed() >= Duration::from_millis(50));
    }
    #[test]
    fn a_wake_from_another_thread_ends_a_sleep_already_under_way() {
        let wake = Arc::new(Wake::new());
        let other = Arc::clone(&wake);
        let started = Instant::now();
        let waker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            other.notify();
        });
        watcher::wait(&[wake.fd()], started + Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_secs(5));
        waker.join().unwrap();
    }
    #[test]
    fn only_power_supply_uevents_count() {
        assert!(power_supply_event(
            b"change@/devices/platform/battery/power_supply/BAT0\0ACTION=change\0SUBSYSTEM=power_supply\0POWER_SUPPLY_STATUS=Discharging\0"
        ));
        assert!(!power_supply_event(b"add@/devices/usb1/1-1\0ACTION=add\0SUBSYSTEM=usb\0"));
        assert!(!power_supply_event(b"change@/x\0ACTION=change\0NOTE=SUBSYSTEM=power_supply_like\0"));
    }
    #[test]
    fn the_power_recheck_is_slow_with_events_and_never_fast_without() {
        let mut power = PowerEvents { uevents: None, profile: None };
        assert_eq!(power.interval(), PowerEvents::WITHOUT_EVENTS);
        assert!(PowerEvents::WITHOUT_EVENTS >= Duration::from_secs(60));
        assert!(PowerEvents::SAFETY >= PowerEvents::WITHOUT_EVENTS);
        assert!(!power.changed());
        // A monitor that exits (no system bus) is dropped, and the loop goes on by the slow re-check.
        let mut child = Command::new("true").stdin(Stdio::null()).stdout(Stdio::piped()).spawn().unwrap();
        let fd = child.stdout.as_ref().unwrap().as_raw_fd();
        child.wait().unwrap();
        power.profile = Some(child);
        watcher::wait(&[Some(fd)], Instant::now() + Duration::from_secs(5));
        assert!(!power.changed());
        assert!(power.profile.is_none());
    }
}
