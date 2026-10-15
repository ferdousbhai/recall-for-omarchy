//! Shared inference and collection isolation use a fake worker, never owner data or network.
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Fixture {
    dir: tempfile::TempDir,
    child: Child,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::copy(env!("CARGO_BIN_EXE_recall"), dir.path().join("recall")).unwrap();
        fs::create_dir_all(dir.path().join("config/recall")).unwrap();
        fs::create_dir(dir.path().join("models")).unwrap();
        for model in recall::model_files::ALL {
            fs::write(dir.path().join("models").join(model.name), "fixture").unwrap();
        }
        fs::write(dir.path().join("config/recall/settings.json"), json!({"roots":[],"code_roots":[],"index_directory":dir.path().join("index"),"models_directory":dir.path().join("models"),"encoder_idle_seconds":1}).to_string()).unwrap();
        let script = format!(
            r#"#!/usr/bin/python3
import sys,json,base64,struct
from pathlib import Path
info={info}
info['text_only']='--text-only' in sys.argv
log=Path(__file__).with_name('worker.log')
with log.open('a') as f:f.write('start\n')
print(json.dumps({{'ready':True,'info':info}}),flush=True)
for line in sys.stdin:
 r=json.loads(line)
 if r['op']=='quit':break
 vectors=[]
 for text in r.get('inputs',[]):
  v=[0.0]*768;v[0 if 'alpha' in text.lower() else 1]=1.0;vectors.extend(v)
 with log.open('a') as f:f.write('embed '+str(len(r.get('inputs',[])))+'\n')
 print(json.dumps({{'id':r['id'],'ok':True,'dim':768,'vectors':base64.b64encode(struct.pack('<'+'f'*len(vectors),*vectors)).decode(),'errors':[None]*len(r.get('inputs',[]))}}),flush=True)
"#,
            info = serde_json::to_string(&recall::llama::model_info())
                .unwrap()
                .replace("false", "False")
                .replace("null", "None")
        );
        fs::write(dir.path().join("recall-engine"), script).unwrap();
        fs::set_permissions(
            dir.path().join("recall-engine"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let child = Command::new(dir.path().join("recall"))
            .arg("daemon")
            .env("HOME", dir.path())
            .env("XDG_RUNTIME_DIR", dir.path())
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("RECALL_OFFLINE", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let fixture = Self { dir, child };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fixture.dir.path().join("recall-for-omarchy.sock").exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        fixture
    }
    fn request(&self, value: Value) -> Value {
        let mut stream =
            UnixStream::connect(self.dir.path().join("recall-for-omarchy.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        writeln!(stream, "{value}").unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
    fn rank(&self, collection: &str, records: Value) -> Value {
        self.request(json!({"api":1,"op":"records.search","collection":collection,"query":"alpha","records":records,"limit":32}))
    }
    fn starts(&self) -> usize {
        fs::read_to_string(self.dir.path().join("worker.log"))
            .unwrap()
            .lines()
            .filter(|s| *s == "start")
            .count()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn one_worker_is_shared_and_app_records_never_cross_scopes() {
    let f = Fixture::new();
    assert_eq!(f.request(json!({"op":"capabilities"}))["api"], 1);
    let records =
        json!([{"id":"a","title":"alpha","text":"alpha"},{"id":"b","title":"beta","text":"beta"}]);
    let ranked = f.rank("mail:work", records.clone());
    assert_eq!(ranked["hits"][0]["id"], "a");
    let log = fs::read_to_string(f.dir.path().join("worker.log")).unwrap();
    assert_eq!(f.rank("mail:work", records)["hits"][0]["id"], "a");
    let next = fs::read_to_string(f.dir.path().join("worker.log")).unwrap();
    assert_eq!(
        next.lines().count(),
        log.lines().count(),
        "repeat query reuses query and document vectors"
    );
    assert_eq!(
        f.rank("notes", json!([{"id":"a","title":"beta","text":"beta"}]))["hits"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let narrow = f.rank(
        "mail:work",
        json!([{"id":"b","title":"beta","text":"beta"}]),
    );
    assert_eq!(narrow["hits"].as_array().unwrap().len(), 1);
    assert_eq!(narrow["hits"][0]["id"], "b");
    assert_eq!(f.starts(), 1, "all collections use the same loaded worker");
    let reply = f.request(json!({"api":1,"op":"embed","kind":"image","inputs":["/fixture.png"]}));
    assert!(reply.get("error").is_none(), "{reply}");
    assert_eq!(
        f.starts(),
        2,
        "media upgrades one worker rather than starting a second resident worker"
    );
    let reply = f.request(json!({"api":1,"op":"embed","kind":"text","inputs":["alpha"]}));
    assert!(reply.get("error").is_none());
    assert_eq!(f.starts(), 2);
    assert_eq!(
        f.request(json!({"api":1,"op":"records.forget","collection":"notes"}))["ok"],
        true
    );
    let db = rusqlite::Connection::open(f.dir.path().join("index/collections.sqlite")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM vectors WHERE collection='notes'",
            [],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        fs::metadata(f.dir.path().join("index/collections.sqlite"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    std::thread::sleep(Duration::from_millis(1300));
    f.request(json!({"api":1,"op":"embed","kind":"text","inputs":["alpha"]}));
    assert_eq!(f.starts(), 3, "idle inference is unloaded");
}
#[test]
fn protocol_errors_do_not_load_models_or_accept_ambiguous_records() {
    let f = Fixture::new();
    assert!(
        f.request(json!({"api":2,"op":"embed","kind":"text","inputs":["alpha"]}))["error"]
            .is_string()
    );
    assert!(f.rank("../escape", json!([]))["error"].is_string());
    assert!(f.rank("mail", json!([{"id":"a"},{"id":"a"}]))["error"].is_string());
    assert!(!f.dir.path().join("worker.log").exists());
    assert_eq!(f.rank("mail", json!([]))["hits"], json!([]));
}

#[test]
fn rank_cli_chunks_records_without_starting_another_worker() {
    let f = Fixture::new();
    let records: Vec<_> = (0..65)
        .map(|i| json!({"id":i.to_string(),"title":"alpha","text":"alpha"}))
        .collect();
    let mut child = Command::new(f.dir.path().join("recall"))
        .args(["rank", "cli:test", "alpha"])
        .env("HOME", f.dir.path())
        .env("XDG_RUNTIME_DIR", f.dir.path())
        .env("XDG_CONFIG_HOME", f.dir.path().join("config"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    serde_json::to_writer(child.stdin.take().unwrap(), &records).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reply["meaning"], true);
    assert_eq!(reply["hits"].as_array().unwrap().len(), 65);
    assert_eq!(f.starts(), 1);
}
