//! The first-party `modal` terminal backend plugin (spec:
//! execution-environments, First-party backend plugins) and the host
//! credential handle it authenticates with (plugin-system, Host credential
//! handles), against fake Modal APIs whose commands run with the local
//! `sh`:
//!
//! - `direct`: a fake of Modal's gRPC control plane (length-prefixed
//!   protobuf over HTTP, failures as trailers-only `grpc-status` headers);
//! - `managed`: a fake of the subscription gateway's REST API.
//!
//! The plugin's manifest grants no secret: the token headers the fakes
//! check can only have been added by the host.

#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Fake, Profile};
use pmt_core::secret::SecretString;
use pmt_plugins::host::ports::{
    Credential, CredentialLookup, HttpPort, HttpRequest, HttpResponse, PortFuture,
};
use pmt_plugins::host::{CallInfo, Runtime};
use pmt_plugins::terminal::registry;
use pmt_sandbox::backend::{ExecRequest, Isolation, RemoteBackend};
use serde_json::{Value, json};

const DIRECT: &str = "https://api.modal.com";
const GATEWAY: &str = "https://modal-gateway.nousresearch.com";
const SERVICE: &str = "/modal.client.ModalClient/";

// --- a minimal protobuf codec for the fake -------------------------------

#[derive(Debug, Clone)]
enum Val {
    Int(u64),
    Bytes(Vec<u8>),
    /// A `float` (the long-poll timeouts).
    Fixed32,
}

fn varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn read_varint(bytes: &[u8], at: &mut usize) -> u64 {
    let mut v = 0;
    let mut shift = 0;
    loop {
        let b = bytes[*at];
        *at += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return v;
        }
        shift += 7;
    }
}

fn decode(bytes: &[u8]) -> Vec<(u32, Val)> {
    let mut at = 0;
    let mut out = Vec::new();
    while at < bytes.len() {
        let key = read_varint(bytes, &mut at);
        let field = u32::try_from(key >> 3).unwrap();
        let value = match key & 7 {
            0 => Val::Int(read_varint(bytes, &mut at)),
            2 => {
                let len = usize::try_from(read_varint(bytes, &mut at)).unwrap();
                at += len;
                Val::Bytes(bytes[at - len..at].to_vec())
            }
            5 => {
                at += 4;
                Val::Fixed32
            }
            wire => panic!("unexpected wire type {wire}"),
        };
        out.push((field, value));
    }
    out
}

struct Msg(Vec<(u32, Val)>);

impl Msg {
    fn of(bytes: &[u8]) -> Self {
        Self(decode(bytes))
    }
    fn bytes(&self, field: u32) -> Vec<u8> {
        self.0
            .iter()
            .rev()
            .find_map(|(f, v)| match v {
                Val::Bytes(b) if *f == field => Some(b.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }
    fn text(&self, field: u32) -> String {
        String::from_utf8(self.bytes(field)).unwrap()
    }
    fn texts(&self, field: u32) -> Vec<String> {
        self.0
            .iter()
            .filter_map(|(f, v)| match v {
                Val::Bytes(b) if *f == field => Some(String::from_utf8(b.clone()).unwrap()),
                _ => None,
            })
            .collect()
    }
    fn int(&self, field: u32) -> u64 {
        self.0
            .iter()
            .rev()
            .find_map(|(f, v)| match v {
                Val::Int(i) if *f == field => Some(*i),
                _ => None,
            })
            .unwrap_or(0)
    }
    fn has_float(&self, field: u32) -> bool {
        self.0
            .iter()
            .any(|(f, v)| *f == field && matches!(v, Val::Fixed32))
    }
    fn message(&self, field: u32) -> Self {
        Self::of(&self.bytes(field))
    }
}

#[derive(Default)]
struct Out(Vec<u8>);

impl Out {
    fn int(mut self, field: u32, v: u64) -> Self {
        varint(&mut self.0, u64::from(field) << 3);
        varint(&mut self.0, v);
        self
    }
    fn bytes(mut self, field: u32, v: &[u8]) -> Self {
        varint(&mut self.0, (u64::from(field) << 3) | 2);
        varint(&mut self.0, v.len() as u64);
        self.0.extend_from_slice(v);
        self
    }
    fn text(self, field: u32, v: &str) -> Self {
        self.bytes(field, v.as_bytes())
    }
    fn message(self, field: u32, v: &Out) -> Self {
        self.bytes(field, &v.0)
    }
}

fn frames(messages: Vec<Out>) -> Vec<u8> {
    let mut body = Vec::new();
    for m in messages {
        body.push(0);
        body.extend_from_slice(&u32::try_from(m.0.len()).unwrap().to_be_bytes());
        body.extend_from_slice(&m.0);
    }
    body
}

fn unframe(body: &[u8]) -> Vec<u8> {
    assert_eq!(body[0], 0, "uncompressed frame");
    let len = u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize;
    assert_eq!(body.len(), 5 + len, "one request message");
    body[5..].to_vec()
}

fn grpc(messages: Vec<Out>) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: vec![("content-type".to_owned(), "application/grpc".to_owned())],
        body: frames(messages),
    }
}

/// A failure as servers send it without a message: status in headers.
fn trailers_only(code: u32, message: &str) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: vec![
            ("content-type".to_owned(), "application/grpc".to_owned()),
            ("grpc-status".to_owned(), code.to_string()),
            ("grpc-message".to_owned(), message.replace(' ', "%20")),
        ],
        body: Vec::new(),
    }
}

fn json_reply(status: u16, body: &Value) -> HttpResponse {
    HttpResponse {
        status,
        headers: Vec::new(),
        body: body.to_string().into_bytes(),
    }
}

// --- the fakes -------------------------------------------------------------

#[derive(Clone, Default)]
struct Exec {
    argv: Vec<String>,
    stdin: Vec<u8>,
    hung: bool,
    ran: Option<(i32, Vec<u8>, Vec<u8>)>,
}

/// Runs `argv` with `stdin` locally.
fn run_local(argv: &[String], stdin: &[u8]) -> (i32, Vec<u8>, Vec<u8>) {
    let mut child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.code().unwrap_or(-1), out.stdout, out.stderr)
}

#[derive(Default)]
struct ModalApi {
    /// `METHOD url-path` (gRPC: the method name) of every request.
    calls: Mutex<Vec<String>>,
    execs: Mutex<BTreeMap<String, Exec>>,
    next: AtomicUsize,
    /// Task id polls answered so far (the first comes back empty).
    task_polls: AtomicUsize,
    /// A command containing `hang` is running.
    hung: AtomicBool,
    /// Idempotency keys of gateway sandbox creates.
    idempotency: Mutex<Vec<String>>,
}

impl ModalApi {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn count(&self, call: &str) -> usize {
        self.calls().iter().filter(|c| *c == call).count()
    }

    fn id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.next.fetch_add(1, Ordering::SeqCst) + 1)
    }

    fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn start(&self, argv: Vec<String>) -> String {
        let id = self.id("ex");
        let hung = argv.iter().any(|a| a.contains("hang"));
        if hung {
            self.hung.store(true, Ordering::SeqCst);
        }
        self.execs.lock().unwrap().insert(
            id.clone(),
            Exec {
                argv,
                hung,
                ..Exec::default()
            },
        );
        id
    }

    /// The exec's result, running it on first ask; `None` while hung.
    fn result(&self, id: &str) -> Option<(i32, Vec<u8>, Vec<u8>)> {
        let mut execs = self.execs.lock().unwrap();
        let exec = execs.get_mut(id).unwrap();
        if exec.hung {
            return None;
        }
        if exec.ran.is_none() {
            exec.ran = Some(run_local(&exec.argv, &exec.stdin));
        }
        exec.ran.clone()
    }

    /// The first task id poll comes back empty, the next names the task.
    fn task_id(&self, m: &Msg) -> HttpResponse {
        assert!(m.has_float(2));
        if self
            .task_polls
            .fetch_add(1, Ordering::SeqCst)
            .is_multiple_of(2)
        {
            return grpc(vec![Out::default()]);
        }
        grpc(vec![Out::default().text(1, &format!("ta-{}", m.text(1)))])
    }

    fn wait(&self, m: &Msg) -> HttpResponse {
        let Some((code, _, _)) = self.result(&m.text(1)) else {
            std::thread::sleep(Duration::from_millis(20));
            return grpc(vec![Out::default()]);
        };
        grpc(vec![
            Out::default()
                .int(1, u64::from(code.cast_unsigned()))
                .int(2, 1),
        ])
    }

    /// One batch with all of the stream's data, then its exit code.
    fn output(&self, m: &Msg) -> HttpResponse {
        let (code, stdout, stderr) = self.result(&m.text(1)).expect("ended");
        let data = if m.int(4) == 1 { stdout } else { stderr };
        assert_eq!(m.int(5), 1, "raw bytes");
        let mut batch = Out::default();
        if m.int(3) == 0 && !data.is_empty() {
            batch = batch.message(1, &Out::default().bytes(3, &data));
        }
        grpc(vec![
            batch.int(2, 1).int(3, u64::from(code.cast_unsigned())),
        ])
    }

    fn grpc(&self, request: &HttpRequest) -> HttpResponse {
        assert_eq!(
            Self::header(request, "content-type"),
            Some("application/grpc")
        );
        assert_eq!(Self::header(request, "te"), Some("trailers"));
        if Self::header(request, "x-modal-token-id") != Some("ak-test")
            || Self::header(request, "x-modal-token-secret") != Some("as-test")
        {
            return trailers_only(16, "invalid token");
        }
        let method = request
            .url
            .strip_prefix(&format!("{DIRECT}{SERVICE}"))
            .unwrap();
        let m = Msg::of(&unframe(request.body.as_deref().unwrap()));
        match method {
            "AppGetOrCreate" => {
                assert_eq!(m.text(1), "prometheus");
                assert_eq!(m.int(3), 1);
                grpc(vec![Out::default().text(1, "ap-1")])
            }
            "ImageGetOrCreate" => {
                assert_eq!(m.text(4), "ap-1");
                assert_eq!(m.message(2).texts(6), ["FROM python:3.12-slim"]);
                // Still building: the plugin joins the build stream.
                grpc(vec![Out::default().text(1, "im-1")])
            }
            "ImageJoinStreaming" => {
                assert_eq!(m.text(1), "im-1");
                grpc(vec![
                    Out::default().text(3, "e1"),
                    Out::default()
                        .message(1, &Out::default().int(1, 1))
                        .text(3, "e2")
                        .int(4, 1),
                ])
            }
            "SandboxCreate" => {
                assert_eq!(m.text(1), "ap-1");
                assert_eq!(m.message(2).text(3), "im-1");
                grpc(vec![Out::default().text(1, &self.id("sb"))])
            }
            "SandboxGetTaskId" => self.task_id(&m),
            "ContainerExec" => {
                assert!(m.text(1).starts_with("ta-"));
                assert_eq!((m.int(6), m.int(7)), (2, 2));
                let id = self.start(m.texts(2));
                grpc(vec![Out::default().text(1, &id)])
            }
            "ContainerExecPutInput" => {
                let input = m.message(2);
                let mut execs = self.execs.lock().unwrap();
                let exec = execs.get_mut(&m.text(1)).unwrap();
                exec.stdin.extend_from_slice(&input.bytes(1));
                grpc(vec![Out::default()])
            }
            "ContainerExecWait" => self.wait(&m),
            "ContainerExecGetOutput" => self.output(&m),
            "SandboxTerminate" => grpc(vec![Out::default()]),
            other => trailers_only(12, &format!("unimplemented {other}")),
        }
    }

    /// A gateway exec: running while hung, else its merged output.
    fn poll(&self, exec: &str) -> HttpResponse {
        let Some((code, stdout, stderr)) = self.result(exec) else {
            std::thread::sleep(Duration::from_millis(20));
            return json_reply(200, &json!({"status": "running"}));
        };
        let mut output = stdout;
        output.extend_from_slice(&stderr);
        json_reply(
            200,
            &json!({"status": "completed", "returncode": code,
                    "output": String::from_utf8_lossy(&output)}),
        )
    }

    fn rest(&self, request: &HttpRequest) -> HttpResponse {
        if Self::header(request, "authorization") != Some("Bearer nous-token") {
            return json_reply(401, &json!({"error": "invalid token"}));
        }
        let path = request.url.strip_prefix(GATEWAY).unwrap();
        let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let body: Value = request
            .body
            .as_deref()
            .and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or(Value::Null);
        match (request.method.as_str(), segments.as_slice()) {
            ("POST", ["v1", "sandboxes"]) => {
                assert_eq!(body["image"], "python:3.12-slim");
                self.idempotency.lock().unwrap().push(
                    Self::header(request, "x-idempotency-key")
                        .unwrap_or_default()
                        .to_owned(),
                );
                json_reply(200, &json!({"id": self.id("ms")}))
            }
            ("POST", ["v1", "sandboxes", _, "execs"]) => {
                let exec = body["execId"].as_str().unwrap().to_owned();
                let argv = vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    body["command"].as_str().unwrap().to_owned(),
                ];
                let hung = argv[2].contains("hang");
                if hung {
                    self.hung.store(true, Ordering::SeqCst);
                }
                let stdin = body["stdinData"]
                    .as_str()
                    .unwrap_or_default()
                    .as_bytes()
                    .to_vec();
                self.execs.lock().unwrap().insert(
                    exec.clone(),
                    Exec {
                        argv,
                        stdin,
                        hung,
                        ran: None,
                    },
                );
                // Answered as running: the plugin polls.
                json_reply(200, &json!({"execId": exec, "status": "running"}))
            }
            ("GET", ["v1", "sandboxes", _, "execs", exec]) => self.poll(exec),
            (
                "POST",
                ["v1", "sandboxes", _, "execs", _, "cancel"] | ["v1", "sandboxes", _, "terminate"],
            ) => json_reply(200, &json!({})),
            _ => json_reply(404, &json!({"error": "no route"})),
        }
    }
}

impl HttpPort for ModalApi {
    fn send(&self, request: HttpRequest) -> PortFuture<'_, Result<HttpResponse, String>> {
        let answer = if let Some(method) = request.url.strip_prefix(&format!("{DIRECT}{SERVICE}")) {
            self.calls.lock().unwrap().push(method.to_owned());
            self.grpc(&request)
        } else {
            let path = request.url.strip_prefix(GATEWAY).unwrap_or(&request.url);
            self.calls
                .lock()
                .unwrap()
                .push(format!("{} {path}", request.method));
            self.rest(&request)
        };
        Box::pin(async move { Ok(answer) })
    }
}

fn direct_credential(token_id: &str) -> CredentialLookup {
    CredentialLookup::Ready(Credential {
        kind: "direct".to_owned(),
        origin: DIRECT.to_owned(),
        billing_source: None,
        hosts: vec!["api.modal.com".to_owned()],
        headers: vec![
            ("x-modal-token-id".to_owned(), SecretString::new(token_id)),
            (
                "x-modal-token-secret".to_owned(),
                SecretString::new("as-test"),
            ),
        ],
    })
}

fn managed_credential() -> CredentialLookup {
    CredentialLookup::Ready(Credential {
        kind: "managed".to_owned(),
        origin: GATEWAY.to_owned(),
        billing_source: Some("subscription:nous".to_owned()),
        hosts: vec!["modal-gateway.nousresearch.com".to_owned()],
        headers: vec![(
            "authorization".to_owned(),
            SecretString::new("Bearer nous-token"),
        )],
    })
}

/// The `modal` plugin with `credential` behind the `modal` handle.
struct Modal {
    api: Arc<ModalApi>,
    fake: Arc<Fake>,
    profile: Profile,
    runtime: Runtime,
}

impl Modal {
    fn new(credential: Option<CredentialLookup>) -> Self {
        let mut profile = Profile::new("p");
        profile.install_shipped("first-party", "modal", "first-party-modal");
        Self::with(profile, credential)
    }

    fn with(profile: Profile, credential: Option<CredentialLookup>) -> Self {
        let fake = Fake::new();
        if let Some(credential) = credential {
            fake.credentials
                .lock()
                .unwrap()
                .insert("modal".to_owned(), credential);
        }
        Self {
            api: Arc::new(ModalApi::default()),
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
        registry.select("modal").unwrap().plugin.clone().unwrap()
    }

    fn work(&self) -> PathBuf {
        let dir = self.profile.path().join("work");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
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

// --- direct mode -------------------------------------------------------------

#[test]
fn direct_mode_runs_a_session_in_one_sandbox_over_grpc() {
    let modal = Modal::new(Some(direct_credential("ak-test")));
    let work = modal.work();
    let plugin = modal.backend("ses_a");
    assert_eq!(plugin.descriptor().isolation, Isolation::Remote);
    assert!(!plugin.descriptor().uses_host_filesystem);
    assert_eq!(plugin.check().unwrap(), format!("mode: direct at {DIRECT}"));

    let first = plugin
        .execute(&request(
            "c1",
            "cd sub && echo \"$GREETING\" && echo warn >&2",
            &work,
        ))
        .unwrap();
    assert_eq!(first.exit_code, Some(0), "{first:?}");
    assert_eq!(first.stdout, "it's me\n");
    assert_eq!(first.stderr, "warn\n");
    assert_eq!(
        first.cwd_observed.as_deref(),
        Some(work.join("sub").to_str().unwrap())
    );
    let failed = plugin.execute(&request("c2", "exit 3", &work)).unwrap();
    assert_eq!(failed.exit_code, Some(3));
    // App, image (built through the join stream) and sandbox once.
    for once in [
        "AppGetOrCreate",
        "ImageGetOrCreate",
        "ImageJoinStreaming",
        "SandboxCreate",
    ] {
        assert_eq!(modal.api.count(once), 1, "{once}: {:?}", modal.api.calls());
    }

    // Another session gets its own sandbox from the cached app and image.
    modal
        .backend("ses_b")
        .execute(&request("c3", "true", &work))
        .unwrap();
    assert_eq!(modal.api.count("SandboxCreate"), 2);
    assert_eq!(modal.api.count("ImageGetOrCreate"), 1);

    plugin.teardown();
    assert_eq!(modal.api.count("SandboxTerminate"), 1);
    plugin.execute(&request("c4", "true", &work)).unwrap();
    assert_eq!(modal.api.count("SandboxCreate"), 3);
}

#[test]
fn direct_mode_moves_files_through_exec_stdin_and_stdout() {
    let modal = Modal::new(Some(direct_credential("ak-test")));
    let work = modal.work();
    let plugin = modal.backend("ses_a");
    let path = work.join("deep/notes.bin");
    let contents: Vec<u8> = (0..=255_u8).cycle().take(700_000).collect();
    plugin.upload(path.to_str().unwrap(), &contents).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), contents);
    assert!(modal.api.count("ContainerExecPutInput") >= 3);
    assert_eq!(plugin.download(path.to_str().unwrap()).unwrap(), contents);
    let missing = work.join("none.txt");
    let why = plugin.download(missing.to_str().unwrap()).unwrap_err();
    assert!(why.contains("not found"), "{why}");
}

#[test]
fn a_rejected_token_comes_back_as_the_grpc_status() {
    let modal = Modal::new(Some(direct_credential("ak-wrong")));
    let work = modal.work();
    let why = modal
        .backend("ses_a")
        .execute(&request("c1", "true", &work))
        .unwrap_err();
    assert!(
        why.contains("Modal AppGetOrCreate failed: UNAUTHENTICATED invalid token"),
        "{why}"
    );
}

#[test]
fn cancel_reaches_a_direct_command_that_is_still_running() {
    let modal = Modal::new(Some(direct_credential("ak-test")));
    let work = modal.work();
    let plugin = modal.backend("ses_a");
    plugin.execute(&request("warm", "true", &work)).unwrap();
    let running = {
        let plugin = Arc::clone(&plugin);
        let hang = request("long", "hang forever", &work);
        std::thread::spawn(move || plugin.execute(&hang))
    };
    while !modal.api.hung.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let quick = plugin.execute(&request("c1", "echo quick", &work)).unwrap();
    assert_eq!(quick.stdout, "quick\n");
    plugin.cancel("long");
    let cancelled = running.join().unwrap().unwrap();
    assert_eq!(cancelled.exit_code, Some(130));
}

// --- managed mode ------------------------------------------------------------

#[test]
fn managed_mode_runs_through_the_subscription_gateway() {
    let modal = Modal::new(Some(managed_credential()));
    let work = modal.work();
    let plugin = modal.backend("ses_a");
    assert_eq!(
        plugin.check().unwrap(),
        format!("mode: managed at {GATEWAY} (billing_source = subscription:nous)")
    );
    let out = plugin
        .execute(&request("c1", "cd sub && echo \"$GREETING\"", &work))
        .unwrap();
    assert_eq!(out.exit_code, Some(0), "{out:?}");
    assert_eq!(out.stdout, "it's me\n");
    assert_eq!(
        out.cwd_observed.as_deref(),
        Some(work.join("sub").to_str().unwrap())
    );
    let path = work.join("up/data.bin");
    plugin
        .upload(path.to_str().unwrap(), b"\x00bin\xffary")
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"\x00bin\xffary");
    assert_eq!(
        plugin.download(path.to_str().unwrap()).unwrap(),
        b"\x00bin\xffary"
    );
    assert!(
        plugin
            .download(work.join("none").to_str().unwrap())
            .unwrap_err()
            .contains("not found")
    );
    assert_eq!(
        modal.api.count("POST /v1/sandboxes"),
        1,
        "{:?}",
        modal.api.calls()
    );
    assert_eq!(
        *modal.api.idempotency.lock().unwrap(),
        ["prometheus-sandbox-ses_a"]
    );
    plugin.teardown();
    assert_eq!(modal.api.count("POST /v1/sandboxes/ms-1/terminate"), 1);
    // No gRPC call went anywhere.
    assert!(modal.api.calls().iter().all(|c| c.contains("/v1/")));
}

#[test]
fn cancel_reaches_a_managed_command_that_is_still_running() {
    let modal = Modal::new(Some(managed_credential()));
    let work = modal.work();
    let plugin = modal.backend("ses_a");
    let running = {
        let plugin = Arc::clone(&plugin);
        let hang = request("long", "hang forever", &work);
        std::thread::spawn(move || plugin.execute(&hang))
    };
    while !modal.api.hung.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(10));
    }
    plugin.cancel("long");
    assert_eq!(running.join().unwrap().unwrap().exit_code, Some(130));
    assert_eq!(
        modal.api.count("POST /v1/sandboxes/ms-1/execs/long/cancel"),
        1
    );
}

// --- the credential handle fails closed ------------------------------------

#[test]
fn an_unavailable_credential_makes_the_backend_unavailable_with_the_reason() {
    let reason = "no Modal secrets for direct mode (store MODAL_TOKEN_ID and MODAL_TOKEN_SECRET with `prometheus secrets set`); no subscription signed in for managed mode (`prometheus auth login nous`)";
    let modal = Modal::new(Some(CredentialLookup::Unavailable(reason.to_owned())));
    let why = modal.backend("ses_a").check().unwrap_err();
    assert_eq!(why, reason);
    assert!(modal.api.calls().is_empty());
}

#[test]
fn an_unknown_handle_is_refused() {
    let modal = Modal::new(None);
    let plugin = modal.backend("ses_a");
    assert_eq!(
        plugin.check().unwrap_err(),
        "no host credential named 'modal'"
    );
    let work = modal.work();
    assert!(plugin.execute(&request("c1", "true", &work)).is_err());
    assert!(modal.api.calls().is_empty());
}

#[test]
fn a_credential_is_never_sent_outside_its_hosts() {
    // The host's credential names another host than the plugin's origin.
    let modal = Modal::new(Some(CredentialLookup::Ready(Credential {
        kind: "direct".to_owned(),
        origin: DIRECT.to_owned(),
        billing_source: None,
        hosts: vec!["elsewhere.example".to_owned()],
        headers: vec![("x-modal-token-id".to_owned(), SecretString::new("ak-test"))],
    })));
    let work = modal.work();
    let why = modal
        .backend("ses_a")
        .execute(&request("c1", "true", &work))
        .unwrap_err();
    assert!(
        why.contains("the credential may not be sent to https://api.modal.com/"),
        "{why}"
    );
    assert!(modal.api.calls().is_empty(), "{:?}", modal.api.calls());
}

#[test]
fn a_plugin_without_the_credentials_grant_is_denied() {
    let mut profile = Profile::new("p");
    profile.install(
        "modal",
        "first-party-modal",
        "\"terminal-backend@1.2\"",
        "network = [\"api.modal.com\"]",
    );
    let modal = Modal::with(profile, Some(direct_credential("ak-test")));
    let why = modal.backend("ses_a").check().unwrap_err();
    assert_eq!(why, "capability_denied: credentials modal");
    assert!(modal.api.calls().is_empty());
}
