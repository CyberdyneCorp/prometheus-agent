//! The channel setup forms' actions against fake adapters (spec:
//! web-dashboard, Channel setup forms): Test and Restart adapter on the
//! debug-build file adapter, `WhatsApp` QR pairing through a stand-in
//! bridge (refreshed QR images streamed until linked, or expiry), and
//! Telegram Create with QR against a fake onboarding service (the token
//! saved and the owner allowlisted; expiry and cancellation save nothing).

#![cfg(all(unix, feature = "dashboard"))]

mod common;

use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use common::dashboard::{self, Signed, call, get, sign_in};
use common::{command, mock_home, ok, run};

/// A foreground daemon with the file adapter and `env`.
struct Daemon(Child);

impl Daemon {
    fn start(home: &Path, env: &[(&str, &str)]) -> Self {
        let child = command(home)
            .args(["daemon", "run"])
            .env("PMT_TEST_ADAPTER_DIR", home.join("adapter"))
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let daemon = Self(child);
        wait("the daemon runs", Duration::from_secs(30), || {
            run(home, &["daemon", "status"]).status.success()
        });
        daemon
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait(what: &str, limit: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct At {
    port: u16,
    signed: Signed,
}

impl At {
    fn open(home: &Path) -> Self {
        let opened = dashboard::open(home, &[]);
        let signed = sign_in(&opened);
        Self {
            port: opened.port,
            signed,
        }
    }

    fn post(&self, path: &str) -> common::api::Answer {
        call(self.port, "POST", path, &self.signed, Some(&json!({})))
    }

    fn state(&self, platform: &str) -> Value {
        get(
            self.port,
            &format!("/api/profiles/default/adapters/{platform}/pairing"),
            &self.signed.cookie,
        )
        .json()
    }

    /// Waits until the pairing of `platform` ends; its outcome.
    fn outcome(&self, platform: &str) -> Value {
        let mut last = Value::Null;
        wait("the pairing ends", Duration::from_secs(30), || {
            last = self.state(platform);
            !matches!(last["state"].as_str(), Some("starting" | "qr" | "code"))
        });
        last
    }
}

/// Test and Restart adapter act on that adapter only, from its saved
/// configuration.
#[test]
fn test_and_restart_act_on_one_adapter() {
    let home = mock_home();
    common::append_config(
        home.path(),
        "\n[gateway.platforms.test]\nenabled = true\nallow_all = true\n",
    );
    let _daemon = Daemon::start(home.path(), &[]);
    let at = At::open(home.path());
    let tested = at.post("/api/profiles/default/adapters/test/test");
    assert_eq!(tested.status, 200, "{tested:?}");
    assert_eq!(tested.json()["ok"], true, "{tested:?}");
    assert_eq!(tested.json()["inbound_only"], true, "{tested:?}");
    let untested = at.post("/api/profiles/default/adapters/myspace/test");
    assert_eq!(untested.status, 200, "{untested:?}");
    assert_eq!(untested.json()["ok"], false, "{untested:?}");
    assert!(
        untested.json()["error"]
            .as_str()
            .is_some_and(|e| e.contains("no adapter for platform 'myspace'")),
        "{untested:?}"
    );
    let restarted = at.post("/api/profiles/default/adapters/test/restart");
    assert_eq!(restarted.status, 200, "{restarted:?}");
    // Freshly connected: `degraded` until the platform confirms it.
    assert!(
        matches!(
            restarted.json()["state"].as_str(),
            Some("connected" | "degraded")
        ),
        "{restarted:?}"
    );
    // A saved change applies on Restart: the fatal marker makes the
    // rebuilt adapter fail for good, so Restart really rebuilt it.
    std::fs::write(home.path().join("adapter/default/fatal"), "").unwrap();
    let failed = at.post("/api/profiles/default/adapters/test/restart");
    assert_eq!(failed.json()["state"], "fatal", "{failed:?}");
    let undeclared = at.post("/api/profiles/default/adapters/nope/pairing");
    assert_eq!(undeclared.status, 404, "{undeclared:?}");
}

/// A bridge that prints `events` as JSON lines, `pause` seconds apart.
fn fake_bridge(home: &Path, events: &[&str], pause: &str) -> std::path::PathBuf {
    let script = home.join("fake-bridge.sh");
    let mut body = String::from("#!/bin/sh\n");
    for event in events {
        let _ = writeln!(body, "echo '{event}'\nsleep {pause}");
    }
    if events.iter().any(|e| e.contains("connected")) {
        body.push_str("exit 0\n");
    } else {
        body.push_str("exec sleep 30\n");
    }
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// `WhatsApp` pairing streams each refreshed QR code as an image until
/// the phone links.
#[test]
fn whatsapp_pairing_streams_refreshed_qr_images_until_linked() {
    let home = mock_home();
    let bridge = fake_bridge(
        home.path(),
        &[
            r#"{"event":"qr","qr":"2@first,abc"}"#,
            r#"{"event":"qr","qr":"2@second,def"}"#,
            r#"{"event":"connected","user":{"id":"551199@s.whatsapp.net","name":"Ana"}}"#,
        ],
        "0.5",
    );
    let bridge = bridge.to_str().unwrap();
    let _daemon = Daemon::start(home.path(), &[("PMT_TEST_WHATSAPP_BRIDGE", bridge)]);
    let at = At::open(home.path());
    let steps = runtime().block_on(async {
        let mut events = dashboard::socket(
            at.port,
            "/api/events",
            &[("Cookie", &at.signed.cookie)],
            &format!("prometheus-events, csrf.{}", at.signed.csrf),
        )
        .await;
        let started = at.post("/api/profiles/default/adapters/whatsapp/pairing");
        assert_eq!(started.status, 202, "{started:?}");
        let again = at.post("/api/profiles/default/adapters/whatsapp/pairing");
        assert_eq!(again.status, 409, "one flow per adapter: {again:?}");
        let mut steps = Vec::new();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(30), events.next())
                .await
                .unwrap();
            let Some(Ok(Message::Text(text))) = next else {
                panic!("{next:?}")
            };
            let event: Value = serde_json::from_str(text.as_str()).unwrap();
            if event["event"] != "adapter.pairing" {
                continue;
            }
            let data = event["data"].clone();
            let done = data["state"] != "qr";
            steps.push(data);
            if done {
                return steps;
            }
        }
    });
    let images: Vec<&str> = steps
        .iter()
        .filter(|s| s["state"] == "qr")
        .map(|s| s["image"].as_str().unwrap())
        .collect();
    assert_eq!(images.len(), 2, "{steps:?}");
    assert!(
        images
            .iter()
            .all(|i| i.starts_with("data:image/svg+xml;base64,"))
    );
    assert_ne!(images[0], images[1], "each refresh is a new code");
    let last = steps.last().unwrap();
    assert_eq!(last["state"], "linked", "{steps:?}");
    assert_eq!(last["account"], "Ana");
    assert_eq!(last["platform"], "whatsapp");
    assert_eq!(at.state("whatsapp")["state"], "linked");
}

/// A phone that never links ends the flow when the window passes.
#[test]
fn whatsapp_pairing_expires_when_nothing_links() {
    let home = mock_home();
    let bridge = fake_bridge(home.path(), &[r#"{"event":"qr","qr":"2@only,abc"}"#], "0.1");
    let _daemon = Daemon::start(
        home.path(),
        &[
            ("PMT_TEST_WHATSAPP_BRIDGE", bridge.to_str().unwrap()),
            ("PMT_TEST_WHATSAPP_PAIR_SECS", "2"),
        ],
    );
    let at = At::open(home.path());
    assert_eq!(
        at.post("/api/profiles/default/adapters/whatsapp/pairing")
            .status,
        202
    );
    let outcome = at.outcome("whatsapp");
    assert_eq!(outcome["state"], "expired", "{outcome:?}");
}

/// The fake onboarding service: a pairing that becomes ready on the
/// `ready_after`-th poll (never with `usize::MAX`).
struct Onboarding {
    port: u16,
    polls: Arc<AtomicUsize>,
    created: Arc<Mutex<Vec<Value>>>,
}

const BOT_TOKEN: &str = "123456789:AAFakeTokenForTheOnboardingTest_-xyz";

/// One request to the fake service: its request line, whether it carried
/// the poll token, and its body.
struct Request {
    line: String,
    authorized: bool,
    body: Vec<u8>,
}

fn read_request(stream: &std::net::TcpStream) -> Request {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut length = 0;
    let mut authorized = false;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        let lower = header.to_ascii_lowercase();
        if let Some(n) = lower.strip_prefix("content-length:") {
            length = n.trim().parse().unwrap();
        }
        authorized |= lower.starts_with("authorization: bearer poll-secret");
        if header.trim().is_empty() {
            break;
        }
    }
    let mut body = vec![0; length];
    std::io::Read::read_exact(&mut reader, &mut body).unwrap();
    Request {
        line,
        authorized,
        body,
    }
}

/// The service's answer: a pairing on create, `ready` from the
/// `ready_after`-th authorized poll on.
fn answer(
    request: &Request,
    ready_after: usize,
    polls: &AtomicUsize,
    created: &Mutex<Vec<Value>>,
) -> Value {
    if request.line.starts_with("POST /v1/telegram/pairings ") {
        created
            .lock()
            .unwrap()
            .push(serde_json::from_slice(&request.body).unwrap());
        return json!({"pairing_id": "p1", "poll_token": "poll-secret",
                      "suggested_username": "my_prometheus_bot",
                      "deep_link": "https://t.me/newbot/SetupBot/my_prometheus_bot"});
    }
    if !(request.line.starts_with("GET /v1/telegram/pairings/p1 ") && request.authorized) {
        return json!({"error": "not found"});
    }
    if polls.fetch_add(1, Ordering::SeqCst) + 1 >= ready_after {
        json!({"status": "ready", "token": BOT_TOKEN,
               "bot_username": "my_prometheus_bot", "owner_user_id": 4242})
    } else {
        json!({"status": "pending"})
    }
}

fn onboarding(ready_after: usize) -> Onboarding {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let polls = Arc::new(AtomicUsize::new(0));
    let created = Arc::new(Mutex::new(Vec::new()));
    let (counted, recorded) = (Arc::clone(&polls), Arc::clone(&created));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let request = read_request(&stream);
            let text = answer(&request, ready_after, &counted, &recorded).to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                text.len()
            );
        }
    });
    Onboarding {
        port,
        polls,
        created,
    }
}

fn telegram_home(service: &Onboarding) -> tempfile::TempDir {
    let home = mock_home();
    common::append_config(
        home.path(),
        &format!(
            "\n[gateway.platforms.telegram.options]\nsetup_service_url = \"http://127.0.0.1:{}\"\n",
            service.port
        ),
    );
    home
}

fn bot_token_set(home: &Path) -> bool {
    let listed = ok(&run(home, &["--json", "secrets", "list"]));
    listed.contains("TELEGRAM_BOT_TOKEN")
}

/// Create with QR stores the token, allowlists only the owner and
/// enables the adapter.
#[test]
fn telegram_create_with_qr_saves_the_token_and_allowlists_the_owner() {
    let service = onboarding(2);
    let home = telegram_home(&service);
    let _daemon = Daemon::start(home.path(), &[]);
    let at = At::open(home.path());
    assert_eq!(
        at.post("/api/profiles/default/adapters/telegram/pairing")
            .status,
        202
    );
    let mut shown = Value::Null;
    wait("the QR code shows", Duration::from_secs(10), || {
        shown = at.state("telegram");
        shown["state"] != "starting"
    });
    assert_eq!(shown["state"], "qr", "{shown:?}");
    assert!(
        shown["image"]
            .as_str()
            .is_some_and(|i| i.starts_with("data:image/svg+xml;base64,"))
    );
    assert_eq!(
        shown["deep_link"],
        "https://t.me/newbot/SetupBot/my_prometheus_bot"
    );
    let outcome = at.outcome("telegram");
    assert_eq!(outcome["state"], "linked", "{outcome:?}");
    assert_eq!(outcome["account"], "my_prometheus_bot");
    assert_eq!(
        service.created.lock().unwrap()[0]["bot_name"],
        "Prometheus Agent"
    );
    assert!(service.polls.load(Ordering::SeqCst) >= 2);
    assert!(bot_token_set(home.path()));
    let allow = ok(&run(
        home.path(),
        &["config", "get", "gateway.platforms.telegram.allow_users"],
    ));
    assert!(allow.contains("4242"), "{allow}");
    let enabled = ok(&run(
        home.path(),
        &["config", "get", "gateway.platforms.telegram.enabled"],
    ));
    assert!(enabled.contains("true"), "{enabled}");
}

/// Scenario: Cancelled setup (desktop-app, Telegram quick setup by QR):
/// a result arriving after the user closed the flow is discarded.
#[test]
fn a_cancelled_telegram_setup_writes_nothing() {
    let service = onboarding(3);
    let home = telegram_home(&service);
    let _daemon = Daemon::start(home.path(), &[]);
    let at = At::open(home.path());
    at.post("/api/profiles/default/adapters/telegram/pairing");
    wait("the QR code shows", Duration::from_secs(10), || {
        at.state("telegram")["state"] == "qr"
    });
    let cancelled = call(
        at.port,
        "DELETE",
        "/api/profiles/default/adapters/telegram/pairing",
        &at.signed,
        None,
    );
    assert_eq!(cancelled.status, 200, "{cancelled:?}");
    std::thread::sleep(Duration::from_secs(5));
    assert_eq!(at.state("telegram")["state"], "cancelled");
    assert!(!bot_token_set(home.path()));
}

/// Expiry leaves the configuration unchanged with the timeout message.
#[test]
fn an_expired_telegram_setup_saves_nothing() {
    let service = onboarding(usize::MAX);
    let home = telegram_home(&service);
    let config = std::fs::read_to_string(home.path().join("profiles/default/config.toml")).unwrap();
    let _daemon = Daemon::start(home.path(), &[("PMT_TEST_TELEGRAM_SETUP_SECS", "3")]);
    let at = At::open(home.path());
    at.post("/api/profiles/default/adapters/telegram/pairing");
    let outcome = at.outcome("telegram");
    assert_eq!(outcome["state"], "expired", "{outcome:?}");
    assert_eq!(
        outcome["message"],
        "Telegram setup timed out; nothing was saved"
    );
    assert!(!bot_token_set(home.path()));
    assert_eq!(
        std::fs::read_to_string(home.path().join("profiles/default/config.toml")).unwrap(),
        config
    );
}

/// One pairing table for every surface: a Telegram flow started from the
/// dashboard is the one the desktop's Messaging pane sees over RPC, a
/// second start there is refused, and a cancel there stops it.
#[test]
fn a_pairing_started_over_rest_is_the_one_the_rpc_panes_see() {
    let service = onboarding(usize::MAX);
    let home = telegram_home(&service);
    let _daemon = Daemon::start(home.path(), &[]);
    let at = At::open(home.path());
    let started = at.post("/api/profiles/default/adapters/telegram/pairing");
    assert_eq!(started.status, 202, "{started:?}");
    wait("the QR code shows", Duration::from_secs(10), || {
        at.state("telegram")["state"] == "qr"
    });
    let params = json!({"profile": "default", "platform": "telegram"});
    let frame = |id: u64, method: &str| json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    let (second, state, cancelled) = runtime().block_on(async {
        let mut ws = dashboard::rpc(at.port, &at.signed).await;
        let second = common::ws::request_on(&mut ws, &frame(1, "adapters.pairing.start")).await;
        let state = common::ws::request_on(&mut ws, &frame(2, "adapters.pairing.state")).await;
        let cancelled = common::ws::request_on(&mut ws, &frame(3, "adapters.pairing.cancel")).await;
        (second, state, cancelled)
    });
    assert_eq!(
        second["error"]["message"], "a pairing is already running for this adapter",
        "{second}"
    );
    assert_eq!(state["result"]["state"], "qr", "{state}");
    assert!(cancelled.get("error").is_none(), "{cancelled}");
    assert_eq!(at.state("telegram")["state"], "cancelled");
}
