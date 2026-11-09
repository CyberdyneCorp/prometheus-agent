//! The first-party `daytona` terminal backend plugin (spec:
//! execution-environments, First-party backend plugins; Remote sandboxes
//! are plugins) against a fake Daytona API whose toolbox runs each command
//! with the local `sh`, so the plugin's script (directory, environment,
//! status, final directory) is exercised for real.

#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Fake, Profile, Seen};
use pmt_plugins::host::ports::{HttpPort, HttpRequest, HttpResponse, PortFuture};
use pmt_plugins::host::{CallInfo, Runtime};
use pmt_plugins::terminal::registry;
use pmt_sandbox::backend::{ExecRequest, Isolation, RemoteBackend};
use serde_json::{Value, json};

const API: &str = "https://app.daytona.io/api";
const KEY: &str = "dk-test";

/// What the fake keeps of one command.
#[derive(Clone)]
struct Ran {
    exit_code: Option<i32>,
    output: String,
}

/// A fake Daytona API.
#[derive(Default)]
struct DaytonaApi {
    /// `METHOD path` of every request.
    calls: Mutex<Vec<String>>,
    /// Sandbox id -> state.
    sandboxes: Mutex<BTreeMap<String, String>>,
    commands: Mutex<BTreeMap<String, Ran>>,
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    next: AtomicUsize,
    /// A command line containing `hang` never ends.
    hung: AtomicBool,
}

fn reply(status: u16, body: &Value) -> HttpResponse {
    HttpResponse {
        status,
        headers: Vec::new(),
        body: body.to_string().into_bytes(),
    }
}

fn raw(status: u16, body: Vec<u8>) -> HttpResponse {
    HttpResponse {
        status,
        headers: Vec::new(),
        body,
    }
}

impl DaytonaApi {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn count(&self, call: &str) -> usize {
        self.calls().iter().filter(|c| *c == call).count()
    }

    fn id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.next.fetch_add(1, Ordering::SeqCst) + 1)
    }

    fn answer(&self, request: &HttpRequest) -> HttpResponse {
        let auth = request
            .headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.as_str());
        if auth != Some(&format!("Bearer {KEY}")) {
            return reply(401, &json!({"message": "invalid API key"}));
        }
        let path = request.url.strip_prefix(API).unwrap_or(&request.url);
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        let segments: Vec<&str> = route.trim_start_matches('/').split('/').collect();
        let body: Value = request
            .body
            .as_deref()
            .and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or(Value::Null);
        match (request.method.as_str(), segments.as_slice()) {
            ("GET", ["sandbox"]) => reply(200, &json!([])),
            ("POST", ["sandbox"]) => {
                let id = self.id("sb");
                self.sandboxes
                    .lock()
                    .unwrap()
                    .insert(id.clone(), "creating".to_owned());
                reply(200, &json!({"id": id, "state": "creating"}))
            }
            ("GET", ["sandbox", id]) => self.sandbox_state(id),
            ("POST", ["sandbox", id, verb]) => {
                let state = if *verb == "stop" {
                    "stopped"
                } else {
                    "started"
                };
                self.sandboxes
                    .lock()
                    .unwrap()
                    .insert((*id).to_owned(), state.to_owned());
                reply(200, &json!({}))
            }
            ("DELETE", ["sandbox", id]) => {
                self.sandboxes.lock().unwrap().remove(*id);
                reply(200, &json!({}))
            }
            (_, ["toolbox", _, "toolbox", rest @ ..]) => {
                self.toolbox(&request.method, rest, query, &body, request)
            }
            _ => reply(404, &json!({"message": "no route"})),
        }
    }

    /// A created sandbox reports `creating` once, then `started`.
    fn sandbox_state(&self, id: &str) -> HttpResponse {
        let mut sandboxes = self.sandboxes.lock().unwrap();
        let Some(state) = sandboxes.get_mut(id) else {
            return reply(404, &json!({"message": "sandbox not found"}));
        };
        let now = state.clone();
        if now == "creating" {
            "started".clone_into(state);
        }
        reply(200, &json!({"id": id, "state": now}))
    }

    fn toolbox(
        &self,
        method: &str,
        rest: &[&str],
        query: &str,
        body: &Value,
        request: &HttpRequest,
    ) -> HttpResponse {
        match (method, rest) {
            ("POST", ["process", "session"]) | ("DELETE", ["process", "session", _]) => {
                reply(200, &json!({}))
            }
            ("POST", ["process", "session", _, "exec"]) => {
                assert_eq!(body["runAsync"], true);
                let id = self.id("cmd");
                let ran = self.run(body["command"].as_str().unwrap());
                self.commands.lock().unwrap().insert(id.clone(), ran);
                reply(200, &json!({"cmdId": id}))
            }
            ("GET", ["process", "session", _, "command", id]) => {
                let ran = self.commands.lock().unwrap()[*id].clone();
                reply(200, &json!({"id": id, "exitCode": ran.exit_code}))
            }
            ("GET", ["process", "session", _, "command", id, "logs"]) => {
                if query.contains("follow=true") {
                    std::thread::sleep(Duration::from_millis(20));
                }
                raw(
                    200,
                    self.commands.lock().unwrap()[*id]
                        .output
                        .clone()
                        .into_bytes(),
                )
            }
            ("POST", ["files", "upload"]) => {
                let contents = multipart_file(request.body.as_deref().unwrap());
                self.files.lock().unwrap().insert(path_of(query), contents);
                reply(200, &json!({}))
            }
            ("GET", ["files", "download"]) => match self.files.lock().unwrap().get(&path_of(query))
            {
                Some(bytes) => raw(200, bytes.clone()),
                None => reply(404, &json!({"message": "file not found"})),
            },
            _ => reply(404, &json!({"message": "no toolbox route"})),
        }
    }

    /// Runs `command` with the local shell, stdout and stderr merged.
    fn run(&self, command: &str) -> Ran {
        if command.contains("hang") {
            self.hung.store(true, Ordering::SeqCst);
            return Ran {
                exit_code: None,
                output: String::new(),
            };
        }
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{command} 2>&1"))
            .output()
            .unwrap();
        Ran {
            exit_code: out.status.code(),
            output: String::from_utf8_lossy(&out.stdout).into_owned(),
        }
    }
}

/// The `path` query value (the plugin encodes `/` as itself).
fn path_of(query: &str) -> String {
    let value = query.strip_prefix("path=").unwrap_or(query);
    value.replace("%20", " ")
}

/// The file part of a one-field multipart body.
fn multipart_file(body: &[u8]) -> Vec<u8> {
    let text = body;
    let start = text.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let end = text.windows(4).rposition(|w| w == b"\r\n--").unwrap();
    text[start..end].to_vec()
}

impl HttpPort for DaytonaApi {
    fn send(&self, request: HttpRequest) -> PortFuture<'_, Result<HttpResponse, String>> {
        let path = request.url.strip_prefix(API).unwrap_or(&request.url);
        self.calls
            .lock()
            .unwrap()
            .push(format!("{} {path}", request.method));
        let answer = self.answer(&request);
        Box::pin(async move { Ok(answer) })
    }
}

/// The `daytona` plugin loaded with `secrets` and its fake API.
struct Daytona {
    api: Arc<DaytonaApi>,
    fake: Arc<Fake>,
    profile: Profile,
    runtime: Runtime,
}

impl Daytona {
    fn new(key: Option<&str>) -> Self {
        let mut profile = Profile::new("p");
        profile.install_shipped("first-party", "daytona", "first-party-daytona");
        let fake = Arc::new(Fake {
            seen: Arc::new(Seen::default()),
            secrets: key
                .map(|k| ("DAYTONA_API_KEY".to_owned(), k.to_owned()))
                .into_iter()
                .collect(),
            settings: Mutex::default(),
            credentials: Mutex::default(),
        });
        Self {
            api: Arc::new(DaytonaApi::default()),
            fake,
            profile,
            runtime: Runtime::new().unwrap(),
        }
    }

    /// The backend as session `session` sees it.
    fn backend(&self, session: &str) -> Arc<dyn RemoteBackend> {
        let mut ports = self.fake.ports();
        ports.http = Arc::clone(&self.api) as Arc<dyn HttpPort>;
        let loaded = self.profile.load(&self.runtime, &ports);
        let call = CallInfo {
            session_id: Some(session.to_owned()),
            surface: "cli".to_owned(),
            cwd: None,
        };
        let (registry, rejected) = registry(&loaded, &call);
        assert!(rejected.is_empty(), "{rejected:?}");
        registry.select("daytona").unwrap().plugin.clone().unwrap()
    }

    fn work(&self) -> PathBuf {
        let dir = self.profile.path().join("work");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        // `pwd` answers the real path.
        dir.canonicalize().unwrap()
    }
}

fn request(id: &str, line: &str, cwd: &Path) -> ExecRequest {
    ExecRequest {
        id: id.to_owned(),
        command: line.to_owned(),
        cwd: cwd.to_string_lossy().into_owned(),
        env: vec![("GREETING".to_owned(), "it's me".to_owned())],
        timeout_ms: u64::MAX,
    }
}

#[test]
fn a_session_runs_its_commands_in_one_sandbox_deleted_at_teardown() {
    let daytona = Daytona::new(Some(KEY));
    let work = daytona.work();
    let plugin = daytona.backend("ses_a");
    assert_eq!(plugin.descriptor().isolation, Isolation::Remote);
    assert!(!plugin.descriptor().uses_host_filesystem);
    assert!(!plugin.descriptor().persistent);
    assert_eq!(plugin.check().unwrap(), format!("Daytona at {API}"));

    let first = plugin
        .execute(&request("c1", "cd sub && echo \"$GREETING\"", &work))
        .unwrap();
    assert_eq!(first.exit_code, Some(0), "{first:?}");
    assert_eq!(first.stdout, "it's me\n");
    assert_eq!(
        first.cwd_observed.as_deref(),
        Some(work.join("sub").to_str().unwrap())
    );
    let failed = plugin
        .execute(&request("c2", "echo no >&2; exit 3", &work))
        .unwrap();
    assert_eq!(failed.exit_code, Some(3));
    assert_eq!(failed.stdout, "no\n");
    // `exit` ends the shell before the directory is reported.
    assert_eq!(failed.cwd_observed, None);
    assert_eq!(
        daytona.api.count("POST /sandbox"),
        1,
        "{:?}",
        daytona.api.calls()
    );

    // Another session gets its own sandbox.
    let other = daytona.backend("ses_b");
    other.execute(&request("c3", "true", &work)).unwrap();
    assert_eq!(daytona.api.count("POST /sandbox"), 2);

    plugin.teardown();
    assert_eq!(
        daytona.api.count("DELETE /sandbox/sb-1"),
        1,
        "{:?}",
        daytona.api.calls()
    );
    assert!(!daytona.api.sandboxes.lock().unwrap().contains_key("sb-1"));
    // A command after teardown starts a fresh sandbox.
    plugin.execute(&request("c4", "true", &work)).unwrap();
    assert_eq!(daytona.api.count("POST /sandbox"), 3);
}

#[test]
fn files_go_up_and_come_back_and_a_missing_file_is_not_found() {
    let daytona = Daytona::new(Some(KEY));
    let plugin = daytona.backend("ses_a");
    plugin
        .upload("/home/daytona/notes.txt", b"hello\r\nworld")
        .unwrap();
    assert_eq!(
        plugin.download("/home/daytona/notes.txt").unwrap(),
        b"hello\r\nworld"
    );
    let missing = plugin.download("/home/daytona/none.txt").unwrap_err();
    assert!(
        missing.contains("/home/daytona/none.txt: not found"),
        "{missing}"
    );
}

#[test]
fn without_its_key_the_backend_is_unavailable_and_says_why() {
    let daytona = Daytona::new(None);
    let plugin = daytona.backend("ses_a");
    let why = plugin.check().unwrap_err();
    assert!(
        why.contains("DAYTONA_API_KEY is not set (prometheus secrets set DAYTONA_API_KEY)"),
        "{why}"
    );
    assert!(daytona.api.calls().is_empty());

    let wrong = Daytona::new(Some("dk-wrong"));
    let why = wrong.backend("ses_a").check().unwrap_err();
    assert!(
        why.contains("Daytona rejected DAYTONA_API_KEY (401)"),
        "{why}"
    );
}

#[test]
fn cancel_reaches_a_command_that_is_still_running() {
    let daytona = Daytona::new(Some(KEY));
    let work = daytona.work();
    let plugin = daytona.backend("ses_a");
    let running = {
        let plugin = Arc::clone(&plugin);
        let hang = request("long", "hang forever", &work);
        std::thread::spawn(move || plugin.execute(&hang))
    };
    while !daytona.api.hung.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(10));
    }
    // Other calls are not queued behind it.
    let quick = plugin.execute(&request("c1", "echo quick", &work)).unwrap();
    assert_eq!(quick.stdout, "quick\n");
    plugin.cancel("long");
    let cancelled = running.join().unwrap().unwrap();
    assert_eq!(cancelled.exit_code, Some(130));
    assert!(
        daytona
            .api
            .calls()
            .iter()
            .any(|c| c.starts_with("DELETE /toolbox/sb-1/toolbox/process/session/__pmt_cwd_long__")),
        "{:?}",
        daytona.api.calls()
    );
}

#[test]
fn a_persistent_sandbox_is_stopped_at_teardown_and_started_again() {
    let daytona = Daytona::new(Some(KEY));
    daytona.fake.set("daytona", "persistent", "true");
    let work = daytona.work();
    let plugin = daytona.backend("ses_a");
    assert!(plugin.descriptor().persistent);
    plugin.execute(&request("c1", "true", &work)).unwrap();
    plugin.teardown();
    assert_eq!(daytona.api.count("POST /sandbox/sb-1/stop"), 1);
    assert_eq!(daytona.api.sandboxes.lock().unwrap()["sb-1"], "stopped");
    plugin.execute(&request("c2", "true", &work)).unwrap();
    assert_eq!(daytona.api.count("POST /sandbox/sb-1/start"), 1);
    assert_eq!(daytona.api.count("POST /sandbox"), 1);
}
