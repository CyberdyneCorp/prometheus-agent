//! The dashboard's API over HTTP and its WebSocket endpoints (spec: web-dashboard):
//! profile-addressed routes, observers among remote users, the live event
//! stream, write-only secrets, schema-driven config edits, redacted and
//! bounded logs, upload limits and the platform support matrix.

#![cfg(feature = "dashboard")]

mod common;

use std::fmt::Write as _;

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use common::api::Request;
use common::dashboard::{self, Signed, call, get, request_on, sign_in};
use common::{daemon, mock_home, ok, run};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn frame(id: u64, method: &str, params: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// A session of `profile` created over the dashboard's client protocol.
fn create_session(
    rt: &tokio::runtime::Runtime,
    port: u16,
    signed: &Signed,
    profile: &str,
) -> String {
    rt.block_on(async {
        let mut ws = dashboard::rpc(port, signed).await;
        let created = request_on(
            &mut ws,
            &frame(1, "session.create", &json!({"profile": profile})),
        )
        .await;
        created["result"]["session_id"].as_str().unwrap().to_owned()
    })
}

fn listed(port: u16, signed: &Signed, profile: &str) -> Vec<String> {
    let answer = get(
        port,
        &format!("/api/profiles/{profile}/sessions"),
        &signed.cookie,
    );
    assert_eq!(answer.status, 200, "{answer:?}");
    answer.json()["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_owned())
        .collect()
}

/// Scenario: Deleting a session. Scenario: Typo in profile.
#[test]
fn profile_routes_touch_only_the_named_profile() {
    let home = mock_home();
    common::api::profile(home.path(), "work", "");
    common::api::multiplex(home.path());
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let signed = sign_in(&opened);
    let rt = runtime();
    let mine = create_session(&rt, opened.port, &signed, "default");
    let work = create_session(&rt, opened.port, &signed, "work");
    let foreign = call(
        opened.port,
        "DELETE",
        &format!("/api/profiles/work/sessions/{mine}"),
        &signed,
        None,
    );
    assert_eq!(
        foreign.status, 404,
        "default's session is not in work: {foreign:?}"
    );
    let deleted = call(
        opened.port,
        "DELETE",
        &format!("/api/profiles/work/sessions/{work}"),
        &signed,
        None,
    );
    assert_eq!(deleted.status, 200, "{deleted:?}");
    assert!(!listed(opened.port, &signed, "work").contains(&work));
    assert_eq!(listed(opened.port, &signed, "default"), vec![mine]);
    let typo = get(opened.port, "/api/profiles/wrok/config", &signed.cookie);
    assert_eq!(typo.status, 404);
    assert_eq!(typo.json(), json!({"error": "unknown profile wrok"}));
}

/// Scenario: OIDC viewer submits (the protocol side): a remote user who
/// is not a driver attaches as an observer and a forced `prompt.submit`
/// is refused with -32002.
#[test]
fn a_remote_user_not_listed_as_driver_cannot_submit() {
    let home = mock_home();
    let issuer = common::oidc::start("viewer@example.com");
    std::fs::write(
        home.path().join("daemon.toml"),
        common::oidc::daemon_toml(
            &issuer,
            &["viewer@example.com", "lead@example.com"],
            &["lead@example.com"],
        ),
    )
    .unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let owner = sign_in(&opened);
    let viewer = dashboard::sign_in_remote(&opened);
    let me = get(opened.port, "/api/session", &viewer.cookie).json();
    assert_eq!(me["role"], "observer", "{me}");
    assert_eq!(me["subject"], "viewer@example.com");
    let rt = runtime();
    let session = create_session(&rt, opened.port, &owner, "default");
    let refused = rt.block_on(async {
        let mut ws = dashboard::rpc(opened.port, &viewer).await;
        let attached = request_on(
            &mut ws,
            &frame(
                1,
                "session.attach",
                &json!({"session_id": session, "role": "driver"}),
            ),
        )
        .await;
        assert!(attached.get("result").is_some(), "{attached}");
        request_on(
            &mut ws,
            &frame(
                2,
                "prompt.submit",
                &json!({"session_id": session, "text": "hi"}),
            ),
        )
        .await
    });
    assert_eq!(refused["error"]["code"], -32002, "{refused}");
    let health = Request::new("GET", "/api/health").send(opened.port).json();
    assert_eq!(health["auth_providers"], json!(["login_link", "oidc"]));
}

/// Scenario: Approval badge (the stream): an approval pending in a
/// session of profile `work` reaches `/api/events` filtered to `work`.
#[test]
fn a_pending_approval_reaches_the_event_stream_of_its_profile() {
    let home = mock_home();
    common::api::profile(home.path(), "work", "");
    common::api::multiplex(home.path());
    let script = home.path().join("script.toml");
    std::fs::write(
        &script,
        "[[rule]]\nuser = \"clean\"\nrepeat = true\n[[rule.tool_calls]]\nname = \"terminal\"\narguments = { command = \"rm -rf build\" }\n",
    )
    .unwrap();
    let work_config = home.path().join("profiles/work/config.toml");
    let mut text = std::fs::read_to_string(&work_config).unwrap();
    let _ = write!(
        text,
        "\n[providers.mock]\nscript = {:?}\n",
        script.to_string_lossy()
    );
    std::fs::write(&work_config, text).unwrap();
    std::fs::create_dir_all(home.path().join("profiles/work/workspace/build")).unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let signed = sign_in(&opened);
    let rt = runtime();
    let event = rt.block_on(async {
        let mut events = dashboard::socket(
            opened.port,
            "/api/events?profiles=work",
            &[("Cookie", &signed.cookie)],
            &format!("prometheus-events, csrf.{}", signed.csrf),
        )
        .await;
        let mut ws = dashboard::rpc(opened.port, &signed).await;
        let created = request_on(
            &mut ws,
            &frame(1, "session.create", &json!({"profile": "work"})),
        )
        .await;
        let session = created["result"]["session_id"].as_str().unwrap().to_owned();
        request_on(
            &mut ws,
            &frame(
                2,
                "prompt.submit",
                &json!({"session_id": session, "text": "clean"}),
            ),
        )
        .await;
        loop {
            let next = tokio::time::timeout(std::time::Duration::from_secs(30), events.next())
                .await
                .unwrap();
            let Some(Ok(Message::Text(text))) = next else {
                panic!("{next:?}")
            };
            let event: Value = serde_json::from_str(text.as_str()).unwrap();
            if event["event"] == "approval.request" {
                ws.send(Message::Close(None)).await.unwrap();
                return (event, session);
            }
        }
    });
    let (event, session) = event;
    assert_eq!(event["profile"], "work");
    assert_eq!(event["session_id"], session.as_str());
    assert!(event["seq"].as_u64().unwrap() > 0);
    assert!(event["data"]["id"].as_str().unwrap().starts_with("srq-"));
}

/// Scenario: Reading a key.
#[test]
fn secrets_are_listed_by_name_and_never_returned() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let signed = sign_in(&opened);
    let value = "sk-dashboard-secret-0123456789";
    let set = call(
        opened.port,
        "PUT",
        "/api/profiles/default/secrets/SERVICE_TOKEN",
        &signed,
        Some(&json!({"value": value})),
    );
    assert_eq!(set.status, 200, "{set:?}");
    let list = get(opened.port, "/api/profiles/default/secrets", &signed.cookie);
    assert_eq!(list.status, 200);
    assert!(
        !list.body.contains(value) && !list.body.contains("sk-dash"),
        "{}",
        list.body
    );
    let row = list.json()["secrets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "SERVICE_TOKEN")
        .cloned()
        .unwrap();
    assert_eq!(row["source"], "file");
    assert!(row["modified"].is_string(), "{row}");
    for path in [
        "/api/profiles/default/secrets/SERVICE_TOKEN",
        "/api/profiles/default/secrets/SERVICE_TOKEN/reveal",
    ] {
        let answer = get(opened.port, path, &signed.cookie);
        assert_eq!(answer.status, 404, "{path}");
        assert!(!answer.body.contains(value));
    }
    let removed = call(
        opened.port,
        "DELETE",
        "/api/profiles/default/secrets/SERVICE_TOKEN",
        &signed,
        None,
    );
    assert_eq!(removed.status, 200);
}

/// Scenario: Invalid value. The schema endpoint serves the running
/// build's typed config schema.
#[test]
fn an_invalid_config_value_is_refused_and_the_file_unchanged() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let signed = sign_in(&opened);
    let file = home.path().join("profiles/default/config.toml");
    let before = std::fs::read(&file).unwrap();
    let refused = call(
        opened.port,
        "PATCH",
        "/api/profiles/default/config",
        &signed,
        Some(&json!({"set": {"busy.input_mode": "maybe"}})),
    );
    assert_eq!(refused.status, 400);
    let message = refused.json()["message"].as_str().unwrap().to_owned();
    for allowed in ["queue", "steer", "interrupt"] {
        assert!(message.contains(allowed), "{message}");
    }
    assert_eq!(std::fs::read(&file).unwrap(), before);
    let saved = call(
        opened.port,
        "PATCH",
        "/api/profiles/default/config",
        &signed,
        Some(&json!({"set": {"busy.input_mode": "steer", "agent.max_iterations": 12}})),
    );
    assert_eq!(saved.status, 200, "{saved:?}");
    assert_eq!(saved.json()["applies"]["busy.input_mode"], "live");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("input_mode = \"steer\""), "{text}");
    let schema = get(
        opened.port,
        "/api/profiles/default/config/schema",
        &signed.cookie,
    )
    .json();
    let printed: Value =
        serde_json::from_str(&ok(&run(home.path(), &["config", "schema"]))).unwrap();
    assert_eq!(schema, printed);
}

/// Scenario: Path traversal. Lines are bounded and records redacted.
#[test]
fn logs_are_named_from_the_registry_bounded_and_redacted() {
    let home = mock_home();
    ok(&common::run_with_stdin(
        home.path(),
        &["secrets", "set", "LOGGED_TOKEN"],
        "tok-in-a-log-line-42",
    ));
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let signed = sign_in(&opened);
    let traversal = get(
        opened.port,
        "/api/logs?file=../../secrets.toml",
        &signed.cookie,
    );
    assert_eq!(traversal.status, 400);
    assert_eq!(traversal.json()["message"], "unknown log file");
    let files = get(opened.port, "/api/logs", &signed.cookie).json();
    assert!(
        files["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "gateway"),
        "{files}"
    );
    let log = home.path().join("profiles/default/logs/prometheus.log");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    let record = json!({"ts": "2026-11-12T00:00:00.000000Z", "level": "info", "component": "agent",
        "profile": "default", "session_id": null, "turn_id": null, "trace_id": null,
        "msg": "called with tok-in-a-log-line-42", "fields": {}});
    let mut lines = String::new();
    for _ in 0..6000 {
        lines.push_str(&record.to_string());
        lines.push('\n');
    }
    std::fs::write(&log, lines).unwrap();
    let tail = get(
        opened.port,
        "/api/logs?file=default/agent&lines=9000",
        &signed.cookie,
    );
    assert_eq!(tail.status, 200, "{tail:?}");
    let records = tail.json()["records"].as_array().unwrap().clone();
    assert_eq!(records.len(), 5000, "at most 5000 lines");
    assert!(!tail.body.contains("tok-in-a-log-line-42"));
    assert!(
        tail.body.contains("[REDACTED:LOGGED_TOKEN]"),
        "{}",
        &tail.body[..400.min(tail.body.len())]
    );
}

/// Sends an upload of `len` bytes, tolerating a server that answers and
/// closes before the body is sent; the raw answer.
fn upload_raw(port: u16, signed: &Signed, len: usize) -> String {
    use std::io::{Read as _, Write as _};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let head = format!(
        "POST /api/profiles/default/uploads HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: {}\r\nX-Prometheus-CSRF: {}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
        signed.cookie, signed.csrf
    );
    stream.write_all(head.as_bytes()).unwrap();
    let chunk = vec![b'x'; 64 * 1024];
    let mut sent = 0;
    while sent < len && stream.write_all(&chunk).is_ok() {
        sent += chunk.len();
    }
    let mut answer = Vec::new();
    let _ = stream.read_to_end(&mut answer);
    String::from_utf8_lossy(&answer).into_owned()
}

/// Scenario: Oversized upload. Downloads serve only referenced blobs.
#[test]
fn an_oversized_upload_is_refused_before_anything_is_stored() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let signed = sign_in(&opened);
    let blobs = home.path().join("profiles/default/blobs");
    let count = || common::files_under(&blobs).len();
    let before = count();
    let refused = upload_raw(opened.port, &signed, 40 * 1024 * 1024);
    assert!(refused.starts_with("HTTP/1.1 413"), "{refused}");
    assert_eq!(count(), before, "no bytes reached the blob store");
    let mut small = Request::new("POST", "/api/profiles/default/uploads")
        .header("Cookie", &signed.cookie)
        .header("X-Prometheus-CSRF", &signed.csrf);
    small.body = Some("hello".to_owned());
    let stored = small.send(opened.port);
    assert_eq!(stored.status, 201, "{stored:?}");
    let blob = stored.json()["blob"]
        .as_str()
        .unwrap()
        .trim_start_matches("blob:sha256:")
        .to_owned();
    let unreferenced = get(
        opened.port,
        &format!("/api/profiles/default/blobs/{blob}"),
        &signed.cookie,
    );
    assert_eq!(unreferenced.status, 404, "no session references it");
}

/// Honest platform support matrix: served at /api/system/platform and in
/// the `rpc.hello` result, each row with a state and a reason.
#[test]
fn the_platform_matrix_is_served_over_http_and_in_the_hello() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let signed = sign_in(&opened);
    let matrix = get(opened.port, "/api/system/platform", &signed.cookie).json();
    let features = matrix["features"].as_array().unwrap();
    let sandbox = features
        .iter()
        .find(|f| f["feature"] == "sandbox.local")
        .unwrap();
    assert!(["supported", "degraded", "unsupported"].contains(&sandbox["state"].as_str().unwrap()));
    assert!(!sandbox["reason"].as_str().unwrap().is_empty());
    let hello = runtime().block_on(async {
        let origin = format!("http://127.0.0.1:{}", opened.port);
        let mut ws = dashboard::socket(
            opened.port,
            "/api/rpc",
            &[("Cookie", &signed.cookie), ("Origin", &origin)],
            &format!("prometheus-rpc.v1, csrf.{}", signed.csrf),
        )
        .await;
        request_on(&mut ws, &frame(0, "rpc.hello", &json!({"client": "dashboard", "client_version": "t",
            "protocol": {"major": 1, "minor": 0}, "capabilities": {"server_requests": false, "events": []}}))).await
    });
    assert_eq!(hello["result"]["platform_support"], matrix, "{hello}");
}
