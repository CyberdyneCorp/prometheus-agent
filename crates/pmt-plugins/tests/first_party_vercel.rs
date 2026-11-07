//! The first-party `vercel` terminal backend plugin (spec:
//! execution-environments, First-party backend plugins; Remote sandboxes
//! are plugins) against a fake Vercel Sandbox API that runs each command
//! with the local `sh` and keeps uploaded files from the gzip tar the
//! plugin sends.

#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Fake, Profile, Seen};
use pmt_plugins::host::ports::{HttpPort, HttpRequest, HttpResponse, PortFuture};
use pmt_plugins::host::{CallInfo, Runtime};
use pmt_plugins::terminal::registry;
use pmt_sandbox::backend::{ExecRequest, Isolation, RemoteBackend};
use serde_json::{Value, json};

const API: &str = "https://api.vercel.com";
const TOKEN: &str = "vt-test";

#[derive(Clone)]
struct Ran {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// A fake Vercel Sandbox API.
#[derive(Default)]
struct VercelApi {
    /// `METHOD path` of every request, without the team query.
    calls: Mutex<Vec<String>>,
    /// Sandbox id -> status.
    sandboxes: Mutex<BTreeMap<String, String>>,
    commands: Mutex<BTreeMap<String, Ran>>,
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    created: Mutex<Vec<Value>>,
    next: AtomicUsize,
    hung: AtomicBool,
}

fn reply(status: u16, body: &Value) -> HttpResponse {
    HttpResponse {
        status,
        headers: Vec::new(),
        body: body.to_string().into_bytes(),
    }
}

impl VercelApi {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn count(&self, call: &str) -> usize {
        self.calls().iter().filter(|c| *c == call).count()
    }

    fn id(&self, prefix: &str) -> String {
        format!("{prefix}_{}", self.next.fetch_add(1, Ordering::SeqCst) + 1)
    }

    fn answer(&self, request: &HttpRequest, route: &str) -> HttpResponse {
        let auth = request.headers.iter().find(|(k, _)| k == "authorization");
        if auth.map(|(_, v)| v.as_str()) != Some(&format!("Bearer {TOKEN}")) {
            return reply(403, &json!({"error": {"message": "Not authorized"}}));
        }
        let body: Value = request
            .body
            .as_deref()
            .and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or(Value::Null);
        let path = route.split('?').next().unwrap();
        let segments: Vec<&str> = path.trim_start_matches("/v1/").split('/').collect();
        match (request.method.as_str(), segments.as_slice()) {
            ("GET", ["sandboxes"]) => reply(200, &json!({"sandboxes": []})),
            ("POST", ["sandboxes"]) => {
                let id = self.id("sbx");
                self.created.lock().unwrap().push(body);
                self.sandboxes
                    .lock()
                    .unwrap()
                    .insert(id.clone(), "pending".to_owned());
                reply(200, &json!({"sandbox": {"id": id, "status": "pending"}}))
            }
            ("GET", ["sandboxes", id]) => self.status(id),
            ("POST", ["sandboxes", id, "stop"]) => {
                self.sandboxes
                    .lock()
                    .unwrap()
                    .insert((*id).to_owned(), "stopped".to_owned());
                reply(200, &json!({}))
            }
            ("POST", ["sandboxes", _, "cmd"]) => {
                let id = self.id("cmd");
                let ran = self.run(&body);
                self.commands.lock().unwrap().insert(id.clone(), ran);
                reply(200, &json!({"command": {"id": id, "exitCode": null}}))
            }
            ("GET", ["sandboxes", _, "cmd", id]) => {
                if !route.contains("wait=true") {
                    return reply(400, &json!({"error": {"message": "expected wait"}}));
                }
                let ran = self.commands.lock().unwrap()[*id].clone();
                if ran.exit_code.is_none() {
                    std::thread::sleep(Duration::from_millis(20));
                }
                reply(
                    200,
                    &json!({"command": {"id": id, "exitCode": ran.exit_code}}),
                )
            }
            ("GET", ["sandboxes", _, "cmd", id, "logs"]) => {
                let ran = self.commands.lock().unwrap()[*id].clone();
                let lines = format!(
                    "{}\n{}\n",
                    json!({"stream": "stdout", "data": ran.stdout}),
                    json!({"stream": "stderr", "data": ran.stderr})
                );
                HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: lines.into_bytes(),
                }
            }
            ("POST", ["sandboxes", _, "cmd", _, "kill"]) => reply(200, &json!({})),
            ("POST", ["sandboxes", _, "fs", "write"]) => self.write(request),
            ("POST", ["sandboxes", _, "fs", "read"]) => {
                match self
                    .files
                    .lock()
                    .unwrap()
                    .get(body["path"].as_str().unwrap())
                {
                    Some(bytes) => HttpResponse {
                        status: 200,
                        headers: Vec::new(),
                        body: bytes.clone(),
                    },
                    None => reply(404, &json!({"error": {"message": "file_not_found"}})),
                }
            }
            _ => reply(404, &json!({"error": {"message": "no route"}})),
        }
    }

    /// A new sandbox reports `pending` once, then `running`.
    fn status(&self, id: &str) -> HttpResponse {
        let mut sandboxes = self.sandboxes.lock().unwrap();
        let Some(status) = sandboxes.get_mut(id) else {
            return reply(404, &json!({"error": {"message": "not found"}}));
        };
        let now = status.clone();
        if now == "pending" {
            "running".clone_into(status);
        }
        reply(200, &json!({"sandbox": {"id": id, "status": now}}))
    }

    /// Unpacks the gzip tar the plugin uploads (paths relative to `x-cwd`).
    fn write(&self, request: &HttpRequest) -> HttpResponse {
        let header = |name: &str| {
            request
                .headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(header("content-type").as_deref(), Some("application/gzip"));
        assert_eq!(header("x-cwd").as_deref(), Some("/"));
        let gz = flate2::read::GzDecoder::new(request.body.as_deref().unwrap());
        let mut archive = tar::Archive::new(gz);
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = format!("/{}", entry.path().unwrap().display());
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            self.files.lock().unwrap().insert(path, bytes);
        }
        reply(200, &json!({}))
    }

    /// Runs `sh -c <script>` locally; a script mentioning `hang` never ends.
    fn run(&self, body: &Value) -> Ran {
        assert_eq!(body["command"], "sh");
        let script = body["args"][1].as_str().unwrap();
        if script.contains("hang") {
            self.hung.store(true, Ordering::SeqCst);
            return Ran {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
            };
        }
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
            .unwrap();
        Ran {
            exit_code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}

impl HttpPort for VercelApi {
    fn send(&self, request: HttpRequest) -> PortFuture<'_, Result<HttpResponse, String>> {
        let path = request
            .url
            .strip_prefix(API)
            .unwrap_or(&request.url)
            .to_owned();
        let (route, team) = path
            .rsplit_once("teamId=")
            .map_or((path.as_str(), ""), |(r, t)| {
                (r.trim_end_matches(['?', '&']), t)
            });
        assert_eq!(team, "team_1", "{path}");
        let route = route.to_owned();
        self.calls.lock().unwrap().push(format!(
            "{} {}",
            request.method,
            route.split('?').next().unwrap()
        ));
        let answer = self.answer(&request, &route);
        Box::pin(async move { Ok(answer) })
    }
}

struct Vercel {
    api: Arc<VercelApi>,
    fake: Arc<Fake>,
    profile: Profile,
    runtime: Runtime,
}

impl Vercel {
    fn new(token: Option<&str>) -> Self {
        let mut profile = Profile::new("p");
        profile.install_shipped("first-party", "vercel", "first-party-vercel");
        let fake = Arc::new(Fake {
            seen: Arc::new(Seen::default()),
            secrets: token
                .map(|t| ("VERCEL_TOKEN".to_owned(), t.to_owned()))
                .into_iter()
                .collect(),
            settings: Mutex::default(),
        });
        fake.set("vercel", "team_id", "\"team_1\"");
        fake.set("vercel", "project_id", "\"prj_1\"");
        Self {
            api: Arc::new(VercelApi::default()),
            fake,
            profile,
            runtime: Runtime::new().unwrap(),
        }
    }

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
        registry.select("vercel").unwrap().plugin.clone().unwrap()
    }

    fn work(&self) -> PathBuf {
        let dir = self.profile.path().join("work");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        dir.canonicalize().unwrap()
    }
}

fn request(id: &str, line: &str, cwd: &std::path::Path) -> ExecRequest {
    ExecRequest {
        id: id.to_owned(),
        command: line.to_owned(),
        cwd: cwd.to_string_lossy().into_owned(),
        env: vec![("GREETING".to_owned(), "it's me".to_owned())],
        timeout_ms: u64::MAX,
    }
}

#[test]
fn a_session_runs_in_one_sandbox_with_split_output_and_stops_it_at_teardown() {
    let vercel = Vercel::new(Some(TOKEN));
    let work = vercel.work();
    let plugin = vercel.backend("ses_a");
    assert_eq!(plugin.descriptor().isolation, Isolation::Remote);
    assert_eq!(plugin.descriptor().default_cwd, "/vercel/sandbox");
    assert!(!plugin.descriptor().persistent);
    assert_eq!(plugin.check().unwrap(), format!("Vercel Sandbox at {API}"));

    let ran = plugin
        .execute(&request(
            "c1",
            "cd sub && echo \"$GREETING\" && echo oops >&2",
            &work,
        ))
        .unwrap();
    assert_eq!(ran.exit_code, Some(0), "{ran:?}");
    assert_eq!(ran.stdout, "it's me\n");
    assert_eq!(ran.stderr, "oops\n");
    assert_eq!(
        ran.cwd_observed.as_deref(),
        Some(work.join("sub").to_str().unwrap())
    );
    let failed = plugin.execute(&request("c2", "exit 4", &work)).unwrap();
    assert_eq!(failed.exit_code, Some(4));
    assert_eq!(
        vercel.api.count("POST /v1/sandboxes"),
        1,
        "{:?}",
        vercel.api.calls()
    );
    let created = vercel.api.created.lock().unwrap()[0].clone();
    assert_eq!(created["projectId"], "prj_1");
    assert_eq!(created["runtime"], "node22");

    plugin.teardown();
    assert_eq!(vercel.api.count("POST /v1/sandboxes/sbx_1/stop"), 1);
    // The next command needs a new sandbox.
    plugin.execute(&request("c3", "true", &work)).unwrap();
    assert_eq!(vercel.api.count("POST /v1/sandboxes"), 2);
}

#[test]
fn a_stopped_sandbox_is_replaced() {
    let vercel = Vercel::new(Some(TOKEN));
    let work = vercel.work();
    let plugin = vercel.backend("ses_a");
    plugin.execute(&request("c1", "true", &work)).unwrap();
    // Vercel ended it at its timeout.
    vercel
        .api
        .sandboxes
        .lock()
        .unwrap()
        .insert("sbx_1".to_owned(), "stopped".to_owned());
    plugin.execute(&request("c2", "true", &work)).unwrap();
    assert_eq!(vercel.api.count("POST /v1/sandboxes"), 2);
}

#[test]
fn files_go_up_as_a_tar_and_come_back() {
    let vercel = Vercel::new(Some(TOKEN));
    let plugin = vercel.backend("ses_a");
    let long = format!("/vercel/sandbox/{}/data.bin", "d".repeat(120));
    let big: Vec<u8> = (0..200_000_u32).map(|i| (i % 251) as u8).collect();
    plugin
        .upload("/vercel/sandbox/notes.txt", b"hello")
        .unwrap();
    plugin.upload(&long, &big).unwrap();
    assert_eq!(
        plugin.download("/vercel/sandbox/notes.txt").unwrap(),
        b"hello"
    );
    assert_eq!(plugin.download(&long).unwrap(), big);
    let missing = plugin.download("/vercel/sandbox/none").unwrap_err();
    assert!(
        missing.contains("/vercel/sandbox/none: not found"),
        "{missing}"
    );
}

#[test]
fn without_its_token_or_project_the_backend_is_unavailable_and_says_why() {
    let vercel = Vercel::new(None);
    let why = vercel.backend("ses_a").check().unwrap_err();
    assert!(
        why.contains("VERCEL_TOKEN is not set (prometheus secrets set VERCEL_TOKEN)"),
        "{why}"
    );
    assert!(vercel.api.calls().is_empty());

    let wrong = Vercel::new(Some("vt-wrong"));
    let why = wrong.backend("ses_a").check().unwrap_err();
    assert!(
        why.contains("Vercel rejected VERCEL_TOKEN (403): Not authorized"),
        "{why}"
    );

    let unset = Vercel::new(Some(TOKEN));
    unset.fake.settings.lock().unwrap().clear();
    let why = unset.backend("ses_a").check().unwrap_err();
    assert!(
        why.contains("plugins.entries.vercel.project_id is not set"),
        "{why}"
    );
}

#[test]
fn cancel_kills_a_command_that_is_still_running() {
    let vercel = Vercel::new(Some(TOKEN));
    let work = vercel.work();
    let plugin = vercel.backend("ses_a");
    let running = {
        let plugin = Arc::clone(&plugin);
        let hang = request("long", "hang forever", &work);
        std::thread::spawn(move || plugin.execute(&hang))
    };
    while !vercel.api.hung.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let quick = plugin.execute(&request("c1", "echo quick", &work)).unwrap();
    assert_eq!(quick.stdout, "quick\n");
    plugin.cancel("long");
    let cancelled = running.join().unwrap().unwrap();
    assert_eq!(cancelled.exit_code, Some(130));
    assert!(
        vercel
            .api
            .calls()
            .iter()
            .any(|c| c.starts_with("POST /v1/sandboxes/sbx_1/cmd/cmd_") && c.ends_with("/kill")),
        "{:?}",
        vercel.api.calls()
    );
}
