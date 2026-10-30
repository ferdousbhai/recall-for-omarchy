//! Native CLI/service contract, isolated from the user's configuration, models and index.
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
const CLI: &str = env!("CARGO_BIN_EXE_recall");
struct Fixture {
    dir: tempfile::TempDir,
    daemon: Option<Child>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("config/recall")).unwrap();
        fs::write(dir.path().join("config/recall/settings.json"),json!({"roots":[],"code_roots":[],"index_directory":dir.path().join("index"),"models_directory":dir.path().join("models")}).to_string()).unwrap();
        Self { dir, daemon: None }
    }
    fn command(&self) -> Command {
        let mut c = Command::new(CLI);
        c.env("HOME", self.dir.path())
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("XDG_DATA_HOME", self.dir.path().join("data"))
            .env("XDG_RUNTIME_DIR", self.dir.path())
            .env("CODEX_HOME", self.dir.path().join("codex"))
            .env("RECALL_OFFLINE", "1");
        c
    }
    fn start(&mut self) {
        self.daemon = Some(
            self.command()
                .arg("daemon")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.dir.path().join("recall-for-omarchy.sock").exists() {
            assert!(Instant::now() < deadline);
            assert!(self.daemon.as_mut().unwrap().try_wait().unwrap().is_none());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn rpc(&self, value: Value) -> Value {
        let mut socket =
            UnixStream::connect(self.dir.path().join("recall-for-omarchy.sock")).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        writeln!(socket, "{value}").unwrap();
        let mut line = String::new();
        BufReader::new(socket).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(c) = &mut self.daemon {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
#[test]
fn skill_is_embedded_and_custom_agent_directories_are_preserved() {
    let f = Fixture::new();
    let output = f.command().arg("skill").output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        include_str!("../omarchy/skills/recall/SKILL.md")
    );
    let custom = f.dir.path().join(".claude/skills/recall");
    fs::create_dir_all(&custom).unwrap();
    fs::write(custom.join("custom"), "keep").unwrap();
    for _ in 0..2 {
        assert!(f
            .command()
            .args(["skill", "install"])
            .output()
            .unwrap()
            .status
            .success());
    }
    assert_eq!(fs::read_to_string(custom.join("custom")).unwrap(), "keep");
    assert!(f.dir.path().join("codex/skills/recall").is_symlink());
}
#[test]
fn cli_validates_before_contacting_service() {
    let f = Fixture::new();
    for arguments in [
        vec!["search", "x", "--kind", "bad"],
        vec!["search", "code: x", "--kind", "doc"],
        vec!["search", "x", "--limit", "101"],
        vec!["text", "0"],
    ] {
        let out = f.command().args(arguments).output().unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert!(serde_json::from_slice::<Value>(&out.stdout)
            .unwrap()
            .get("error")
            .is_some());
    }
    let output = f.command().arg("status").output().unwrap();
    assert_eq!(output.status.code(), Some(1));
}
#[test]
fn native_socket_pause_config_and_exclusive_daemon() {
    let mut f = Fixture::new();
    let folder = f.dir.path().join("files");
    fs::create_dir(&folder).unwrap();
    let mut config: Value = serde_json::from_slice(
        &fs::read(f.dir.path().join("config/recall/settings.json")).unwrap(),
    )
    .unwrap();
    config["roots"] = json!([folder]);
    fs::write(
        f.dir.path().join("config/recall/settings.json"),
        config.to_string(),
    )
    .unwrap();
    f.start();
    let socket = f.dir.path().join("recall-for-omarchy.sock");
    assert_eq!(
        fs::metadata(socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(f.rpc(json!({"op":"pause","paused":true,"id":42}))["id"], 42);
    assert_eq!(f.rpc(json!({"op":"status"}))["paused"], true);
    assert_eq!(f.rpc(json!({"op":"pause","paused":false}))["ok"], true);
    assert!(f
        .rpc(json!({"op":"search","query":"x","kinds":["invalid"]}))
        .get("error")
        .is_some());
    assert!(f
        .rpc(json!({"op":"open","hit":{"path":"/etc/passwd"}}))
        .get("error")
        .is_some());
    assert!(f
        .rpc(json!({"op":"configure","config":{"roots":["relative"]}}))
        .get("error")
        .is_some());
    assert_eq!(
        f.rpc(json!({"op":"configure","config":{"encoder_idle_seconds":30}}))["ok"],
        true
    );
    let saved: Value = serde_json::from_slice(
        &fs::read(f.dir.path().join("config/recall/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["encoder_idle_seconds"], 30);
    let output = f.command().arg("daemon").output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(f.rpc(json!({"op":"status"}))["paused"], false);
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.rpc(json!({"op":"config"}))["config"]["encoder_idle_seconds"] != 30 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = f
        .command()
        .args(["search", "nothing", "--keyword-only"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let pid = f.daemon.as_ref().unwrap().id();
    let workers = || -> Vec<String> {
        let mut pids: Vec<_> = fs::read_dir(format!("/proc/{pid}/task"))
            .unwrap()
            .flatten()
            .flat_map(|task| {
                fs::read_to_string(task.path().join("children"))
                    .unwrap_or_default()
                    .split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect();
        pids.retain(|pid| {
            fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|bytes| {
                let command = String::from_utf8_lossy(&bytes);
                command.split('\0').nth(1) == Some("serve")
            })
        });
        pids.sort();
        pids
    };
    let before = workers();
    assert!(!before.is_empty());
    std::thread::sleep(Duration::from_millis(100));
    assert!(f
        .command()
        .args(["search", "nothing", "--keyword-only"])
        .output()
        .unwrap()
        .status
        .success());
    assert_eq!(
        before,
        workers(),
        "Search worker must outlive the request thread"
    );
}

#[test]
fn result_opening_preserves_media_offsets_and_code_lines_without_shell_interpolation() {
    let mut f = Fixture::new();
    let files = f.dir.path().join("files");
    fs::create_dir(&files).unwrap();
    let file = files.join("a # $(not-a-command).txt");
    fs::write(&file, "source").unwrap();
    let mut config: Value = serde_json::from_slice(
        &fs::read(f.dir.path().join("config/recall/settings.json")).unwrap(),
    )
    .unwrap();
    config["roots"] = json!([files]);
    config["code_roots"] = json!([files]);
    config["code_editor"] = json!("nvim");
    fs::write(
        f.dir.path().join("config/recall/settings.json"),
        config.to_string(),
    )
    .unwrap();
    let bin = f.dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let log = f.dir.path().join("open.log");
    for name in ["mpv", "xdg-terminal-exec"] {
        let command = bin.join(name);
        fs::write(&command, "#!/bin/sh\nlog=\"${RECALL_TEST_OPEN_LOG}.${0##*/}\"\nprintf '%s\\n' \"$@\" > \"$log\"\n").unwrap();
        fs::set_permissions(command, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut daemon = f.command();
    daemon
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("RECALL_TEST_OPEN_LOG", &log);
    f.daemon = Some(
        daemon
            .arg("daemon")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f.dir.path().join("recall-for-omarchy.sock").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        f.rpc(json!({"op":"open","hit":{"path":file,"segment":"audio","loc":42.5}}))["ok"],
        true
    );
    assert_eq!(f.rpc(json!({"op":"open","hit":{"path":file,"segment":"lines","loc":1,"focus":14,"code":true}}))["ok"],true);
    let media_log = log.with_extension("log.mpv");
    let editor_log = log.with_extension("log.xdg-terminal-exec");
    loop {
        if fs::read_to_string(&media_log)
            .unwrap_or_default()
            .lines()
            .count()
            == 2
            && fs::read_to_string(&editor_log)
                .unwrap_or_default()
                .lines()
                .count()
                == 3
        {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        fs::read_to_string(media_log).unwrap(),
        format!("--start=42.5\n{}\n", file.display())
    );
    assert_eq!(
        fs::read_to_string(editor_log).unwrap(),
        format!("nvim\n+14\n{}\n", file.display())
    );
}

#[test]
fn replacing_the_cli_binary_does_not_break_a_running_coordinator() {
    let mut fixture = Fixture::new();
    let selected = fixture.dir.path().join("selected");
    fs::create_dir(&selected).unwrap();
    let settings = fixture.dir.path().join("config/recall/settings.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    config["roots"] = json!([selected]);
    fs::write(settings, config.to_string()).unwrap();
    let directory = fixture.dir.path().join("bundle/target/release");
    fs::create_dir_all(&directory).unwrap();
    let binary = directory.join("recall");
    fs::copy(CLI, &binary).unwrap();
    fs::copy(
        env!("CARGO_BIN_EXE_recall-engine"),
        directory.join("recall-engine"),
    )
    .unwrap();
    let environment = fixture.command();
    let mut daemon = Command::new(&binary);
    daemon.envs(
        environment
            .get_envs()
            .filter_map(|(k, v)| v.map(|v| (k, v))),
    );
    daemon
        .arg("daemon")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let deadline = Instant::now() + Duration::from_secs(2);
    fixture.daemon = Some(loop {
        // Parallel fork/exec tests can briefly inherit a writer to the freshly copied ELF.
        match daemon.spawn() {
            Ok(child) => break child,
            Err(error)
                if error.raw_os_error() == Some(libc::ETXTBSY) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("Could not launch copied CLI: {error}"),
        }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fixture.dir.path().join("recall-for-omarchy.sock").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let old = directory.join("recall.before");
    fs::rename(&binary, &old).unwrap();
    fs::copy(CLI, &binary).unwrap();
    fs::remove_file(old).unwrap();
    let response = fixture.rpc(json!({"op":"search","query":"nothing","meaning":false}));
    assert!(response.get("error").is_none(), "{response}");
}

#[test]
fn empty_selections_and_exclusions_hide_retained_hits_and_passages() {
    use recall::{
        crawler::Candidate,
        model::{FileKind, Modality, SegmentKind},
        store::{Store, MAIN_FILE},
    };
    let mut fixture = Fixture::new();
    let folder = fixture.dir.path().join("documents");
    fs::create_dir(&folder).unwrap();
    let file = folder.join("retained.txt");
    fs::write(&file, "retained secret passage").unwrap();
    let store = Store::open(&fixture.dir.path().join("index"), MAIN_FILE).unwrap();
    let id = store
        .insert(&Candidate {
            path: file.to_string_lossy().into_owned(),
            kind: FileKind::Doc,
            size: 23,
            mtime: 1.0,
            inode: 1,
            device: 1,
        })
        .unwrap();
    store
        .add_segment(
            id,
            SegmentKind::Chunk,
            Modality::Text,
            Some(1.0),
            None,
            None,
            Some("retained secret passage"),
            None,
        )
        .unwrap();
    let segment: i64 = store
        .db
        .query_row(
            "SELECT id FROM segments WHERE file=? AND kind='chunk'",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    fixture.start();
    assert!(
        fixture.rpc(json!({"op":"search","query":"retained","meaning":false}))["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(fixture.rpc(json!({"op":"text","segment":segment}))["text"].is_null());
    assert_eq!(
        fixture
            .rpc(json!({"op":"configure","config":{"roots":[fixture.dir.path()],"excluded_folders":[folder]}}))
            ["ok"],
        true
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while fixture.rpc(json!({"op":"config"}))["config"]["roots"]
        .as_array()
        .unwrap()
        .is_empty()
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        fixture.rpc(json!({"op":"search","query":"retained","meaning":false}))["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(fixture.rpc(json!({"op":"text","segment":segment}))["text"].is_null());
}

/// Copies the CLI and engine into an installation laid out like the package (`<root>/target/release`, `<root>/omarchy`).
fn installation(at: &std::path::Path) -> std::path::PathBuf {
    let release = at.join("target/release");
    fs::create_dir_all(&release).unwrap();
    fs::copy(CLI, release.join("recall")).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_recall-engine"), release.join("recall-engine")).unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("omarchy");
    for folder in ["plugin", "assets"] {
        fs::create_dir_all(at.join("omarchy").join(folder)).unwrap();
        for entry in fs::read_dir(source.join(folder)).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), at.join("omarchy").join(folder).join(entry.file_name())).unwrap();
        }
    }
    release.join("recall")
}
/// Runs a freshly copied executable, retrying while a parallel fork still holds it open for writing.
fn spawn_copied(command: &mut Command) -> Child {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match command.spawn() {
            Ok(child) => return child,
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("Could not launch copied CLI: {error}"),
        }
    }
}

#[test]
fn setup_takes_over_a_tarball_install_and_keeps_settings_and_index() {
    let mut fixture = Fixture::new();
    let home = fixture.dir.path().to_path_buf();
    // Desktop commands are stubs that record their arguments.
    let stubs = home.join("stubs");
    fs::create_dir_all(&stubs).unwrap();
    let log = home.join("commands.log");
    for name in ["qs", "systemctl", "hyprctl", "omarchy-shell", "omarchy"] {
        let path = stubs.join(name);
        fs::write(&path, format!("#!/bin/sh\necho \"{name} $*\" >> '{}'\n", log.display())).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let shell = home.join("omarchy-shell-root");
    fs::create_dir_all(&shell).unwrap();
    // What a v0.4.3 tarball install left behind.
    let old = home.join("data/recall-for-omarchy/releases/v0.4.3/recall-for-omarchy/target/release");
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join("recall"), "old").unwrap();
    let bin = home.join(".local/bin");
    fs::create_dir_all(&bin).unwrap();
    let quoted = |name: &str| format!("'{}'", old.join(name).display());
    fs::write(bin.join("recall"), format!("#!/bin/sh\nexec {} \"$@\"\n", quoted("recall"))).unwrap();
    fs::write(bin.join("recall-client"), format!("#!/bin/sh\nexec {} client \"$@\"\n", quoted("recall"))).unwrap();
    fs::write(bin.join("recall-engine"), format!("#!/bin/sh\nexec {} \"$@\"\n", quoted("recall-engine"))).unwrap();
    fs::write(bin.join("recall-shell"), "#!/bin/sh\nexport OMARCHY_PATH='/usr/share/omarchy'\nexec omarchy-shell \"$@\"\n").unwrap();
    fs::write(bin.join("unrelated"), "#!/bin/sh\necho mine\n").unwrap();
    let units = home.join("config/systemd/user");
    fs::create_dir_all(&units).unwrap();
    fs::write(units.join("recall-for-omarchy.service"), format!("[Unit]\nDescription=Recall local semantic search for Omarchy\nPartOf=graphical-session.target\nAfter=graphical-session.target\n\n[Service]\nType=simple\nExecStart=\"{}\" daemon\nRestart=on-failure\nRestartSec=5\nNice=10\nUMask=0077\nTimeoutStopSec=15\n\n[Install]\nWantedBy=graphical-session.target\n", old.join("recall").display())).unwrap();
    let plugin = home.join("config/omarchy/plugins/ferdous.recall");
    fs::create_dir_all(&plugin).unwrap();
    fs::write(plugin.join("SearchPanel.qml"), "old panel").unwrap();
    let bindings = home.join("config/hypr/bindings.lua");
    fs::create_dir_all(bindings.parent().unwrap()).unwrap();
    let keys = "-- recall-for-omarchy: start\no.bind(\"SUPER + CTRL + SHIFT + F\", \"Recall semantic search\", \"recall toggle\")\n-- recall-for-omarchy: end\n";
    fs::write(&bindings, keys).unwrap();
    let settings = home.join("config/recall/settings.json");
    let before = fs::read(&settings).unwrap();
    fs::create_dir_all(home.join("index")).unwrap();
    fs::write(home.join("index/index.sqlite"), "index").unwrap();
    fs::create_dir_all(home.join("data/recall/Models")).unwrap();
    fs::write(home.join("data/recall/Models/model.gguf"), "model").unwrap();

    let installed = installation(&home.join("usr/lib/recall-for-omarchy"));
    let path = format!("{}:{}", stubs.display(), std::env::var("PATH").unwrap());
    let environment = fixture.command();
    let mut setup = Command::new(&installed);
    setup
        .envs(environment.get_envs().filter_map(|(k, v)| v.map(|v| (k, v))))
        .env("PATH", &path)
        .env("OMARCHY_PATH", &shell)
        .arg("setup")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = spawn_copied(&mut setup).wait_with_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    let installed = fs::canonicalize(&installed).unwrap();
    let engine = installed.with_file_name("recall-engine");
    let text = |path: &std::path::Path| fs::read_to_string(path).unwrap();
    assert_eq!(text(&bin.join("recall")), format!("#!/bin/sh\nexec '{}' \"$@\"\n", installed.display()));
    assert_eq!(text(&bin.join("recall-client")), format!("#!/bin/sh\nexec '{}' client \"$@\"\n", installed.display()));
    assert_eq!(text(&bin.join("recall-engine")), format!("#!/bin/sh\nexec '{}' \"$@\"\n", engine.display()));
    assert!(text(&bin.join("recall-shell")).contains(&shell.display().to_string()));
    assert_eq!(text(&bin.join("unrelated")), "#!/bin/sh\necho mine\n");
    let leftovers: Vec<_> = fs::read_dir(&bin).unwrap().chain(fs::read_dir(&units).unwrap())
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".bak."))
        .collect();
    assert!(leftovers.is_empty(), "our own launchers and unit need no backups: {leftovers:?}");
    assert!(text(&units.join("recall-for-omarchy.service")).contains(&format!("ExecStart=\"{}\" daemon", installed.display())));
    assert!(!home.join("data/recall-for-omarchy").exists(), "the unused tarball releases are removed");
    assert_eq!(fs::read(&settings).unwrap(), before);
    assert_eq!(text(&home.join("index/index.sqlite")), "index");
    assert_eq!(text(&home.join("data/recall/Models/model.gguf")), "model");
    assert_eq!(text(&bindings), keys);
    assert_eq!(text(&plugin.join("SearchPanel.qml")), include_str!("../omarchy/plugin/SearchPanel.qml"));
    assert!(!fs::read_dir(&plugin).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains(".bak.")));
    let commands = text(&log);
    assert!(commands.contains("systemctl --user restart recall-for-omarchy.service"), "{commands}");
    assert!(commands.contains("omarchy plugin enable ferdous.recall"), "{commands}");
    assert!(!commands.contains("hyprctl"), "the existing keybinding is kept: {commands}");

    // A package update replaces the files; the restarted service refreshes the user's copies.
    fs::write(plugin.join("SearchPanel.qml"), "panel from the previous version").unwrap();
    let skill = home.join(".agents/skills/recall/SKILL.md");
    fs::write(&skill, "skill from the previous version").unwrap();
    let mut daemon = Command::new(&installed);
    daemon
        .envs(environment.get_envs().filter_map(|(k, v)| v.map(|v| (k, v))))
        .env("PATH", &path)
        .env("OMARCHY_PATH", &shell)
        .arg("daemon")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    fixture.daemon = Some(spawn_copied(&mut daemon));
    let deadline = Instant::now() + Duration::from_secs(10);
    while text(&skill) != include_str!("../omarchy/skills/recall/SKILL.md")
        || text(&plugin.join("SearchPanel.qml")) != include_str!("../omarchy/plugin/SearchPanel.qml")
    {
        assert!(Instant::now() < deadline, "the service did not refresh the panel and skill");
        std::thread::sleep(Duration::from_millis(20));
    }
}
