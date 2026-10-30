use super::*;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
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
    config: Mutex<Value>,
    status: Mutex<Value>,
    rpc: Mutex<Option<Rpc>>,
    helper: Mutex<Option<Helper>>,
    dirty: AtomicBool,
    sync: AtomicBool,
    paused: AtomicBool,
    connections: AtomicUsize,
}
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
                json!({"api":1,"operations":["embed","records.search","records.forget"],"source":recall::llama::model_info().vector_source(),"models_directory":self.config.lock().unwrap()["models_directory"],"batch_size":32}),
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
                }
                result
            }

            "status" => {
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
                        .map(|file| {
                            fs::metadata(folder.join(file.name))
                                .or_else(|_| {
                                    fs::metadata(folder.join(format!("{}.part", file.name)))
                                })
                                .map(|m| m.len().min(file.bytes))
                                .unwrap_or(0)
                        })
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
                        writeln!(h.input, "stop")?;
                        h.stopping = true;
                    }
                } else {
                    self.sync.store(true, Ordering::Release);
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
    fn start_helper(self: &Arc<Self>, backfill: bool) -> Result<()> {
        let config = self.config.lock().unwrap().clone();
        if self.paused.load(Ordering::Acquire)
            || (folders(&config, "roots").is_empty() && folders(&config, "code_roots").is_empty())
        {
            return Ok(());
        }
        let mut holder = self.helper.lock().unwrap();
        if let Some(h) = holder.as_mut() {
            if h.child.try_wait()?.is_none() {
                if h.stopping {
                    self.sync.store(true, Ordering::Release);
                    return Ok(());
                }
                writeln!(h.input, "sync")?;
                writeln!(h.input, "sync code")?;
                return Ok(());
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
        Ok(())
    }
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
    let saver = [
        (
            "org.freedesktop.UPower.PowerProfiles",
            "/org/freedesktop/UPower/PowerProfiles",
        ),
        ("net.hadess.PowerProfiles", "/net/hadess/PowerProfiles"),
    ]
    .iter()
    .find_map(|(service, path)| {
        Command::new("busctl")
            .args([
                "--system",
                "--timeout=1s",
                "get-property",
                service,
                path,
                service,
                "ActiveProfile",
            ])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).contains("\"power-saver\""))
    })
    .unwrap_or(false);
    !saver
}
fn download(state: Arc<State>) {
    thread::spawn(move || loop {
        let config = state.config.lock().unwrap().clone();
        let folder = Path::new(config["models_directory"].as_str().unwrap());
        let wanted = if !folders(&config, "roots").is_empty()
            || !folders(&config, "code_roots").is_empty()
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
            thread::sleep(Duration::from_secs(5));
            continue;
        }
        state.status.lock().unwrap()["download"] = json!("downloading");
        let result = (|| -> Result<()> {
            fs::create_dir_all(folder)?;
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(folder.join(".download.lock"))?;
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(io::Error::last_os_error().into());
            }
            for file in missing {
                if folder.join(file.name).is_file() {
                    continue;
                }
                let temporary = folder.join(format!("{}.part", file.name));
                if let Some(cached) = model_files::cached_copy(&file) {
                    if model_files::sha256(&cached)? != file.sha256 {
                        bail!("Cached model checksum verification failed");
                    }
                    if temporary.exists() {
                        fs::remove_file(&temporary)?;
                    }
                    if fs::hard_link(&cached, &temporary).is_err() {
                        fs::copy(cached, &temporary)?;
                    }
                } else {
                    checked(
                        Command::new("curl")
                            .args([
                                "-fL",
                                "--retry",
                                "3",
                                "--connect-timeout",
                                "15",
                                "--speed-limit",
                                "1024",
                                "--speed-time",
                                "60",
                                "-C",
                                "-",
                                "-o",
                            ])
                            .arg(&temporary)
                            .arg(file.url())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null()),
                    )?;
                }
                if temporary.metadata()?.len() != file.bytes
                    || model_files::sha256(&temporary)? != file.sha256
                {
                    fs::remove_file(temporary)?;
                    bail!("Model checksum verification failed");
                }
                fs::rename(temporary, folder.join(file.name))?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                state.status.lock().unwrap()["download"] = json!("ready");
                state.sync.store(true, Ordering::Release);
            }
            Err(e) => {
                let mut s = state.status.lock().unwrap();
                s["download"] = json!("retrying");
                s["line"] = json!(format!("Model download: {e}"));
            }
        }
        thread::sleep(Duration::from_secs(30));
    });
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
    if std::env::var("RECALL_OFFLINE").as_deref() != Ok("1") {
        download(Arc::clone(&state));
    } else {
        state.status.lock().unwrap()["download"] = json!(if model_files::missing(Path::new(
            config["models_directory"].as_str().unwrap()
        ))
        .is_empty()
        {
            "ready"
        } else {
            "offline"
        });
    }
    let background = Arc::clone(&state);
    thread::spawn(move || {
        let mut config = config;
        let mut watcher = watcher::Watcher::new(&config);
        let mut policy = backfill_allowed(&config);
        let mut config_check = Instant::now();
        let mut power_check = Instant::now();
        loop {
            if config_check.elapsed() >= Duration::from_secs(1) {
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
                        config = new;
                        *background.config.lock().unwrap() = config.clone();
                        let _ = fs::create_dir_all(config["index_directory"].as_str().unwrap());
                        watcher = watcher::Watcher::new(&config);
                        background.sync.store(true, Ordering::Release);
                    }
                    Err(e) => {
                        eprintln!("Settings: {e}");
                    }
                    _ => (),
                }
            }
            if power_check.elapsed() >= Duration::from_secs(10) {
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
            if background.sync.swap(false, Ordering::AcqRel) {
                if let Err(e) = background.start_helper(policy) {
                    eprintln!("Indexer: {e}");
                    background.sync.store(true, Ordering::Release);
                }
            }
            thread::sleep(Duration::from_millis(250));
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
