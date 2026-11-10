//! The daemon's observability surface against real daemon processes on
//! temporary homes (spec: observability, Local metrics scrape endpoint;
//! Metrics catalog; Health endpoints; Component health model; Live usage
//! events; Status command).

#![cfg(feature = "gateway")]

mod common;

use std::io::{Read as _, Write as _};
use std::path::Path;

use pmt_rpc::contracts as c;

use self::common::daemon::{self, connect, record};
use self::common::{append_config, mock_home};

/// `GET <health_url><path>`: the status code and body.
fn get(home: &Path, path: &str) -> (u16, String) {
    let url = record(home)
        .endpoint
        .health_url
        .expect("health_url in gateway.json");
    let addr = url.trim_start_matches("http://");
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    (status, body)
}

#[test]
fn scrape_is_disabled_by_default() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let (status, _) = get(home.path(), "/metrics");
    assert_eq!(status, 404);
    let (status, body) = get(home.path(), "/healthz");
    assert_eq!((status, body.as_str()), (200, r#"{"status":"ok"}"#));
}

#[test]
fn an_enabled_scrape_serves_the_catalog_without_session_or_user_labels() {
    let home = mock_home();
    append_config(
        home.path(),
        "\n[observability.metrics_endpoint]\nenabled = true\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let (status, body) = get(home.path(), "/metrics");
    assert_eq!(status, 200, "{body}");
    for line in [
        "# TYPE prometheus_tokens_total counter",
        "# TYPE prometheus_model_latency_ms histogram",
        "# TYPE prometheus_sessions_active gauge",
    ] {
        assert!(body.contains(line), "{body}");
    }
    for label in ["session_id", "profile=", "user", "chat_id"] {
        assert!(!body.contains(label), "{label} in {body}");
    }
}

#[test]
fn system_metrics_reads_the_catalog_over_rpc() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let snapshot = runtime.block_on(async {
        let (client, _inbox) = connect(home.path(), "default").await;
        client
            .call_typed::<c::SystemMetricsCall>(&c::SystemParams::default())
            .await
            .unwrap()
    });
    let names: Vec<&str> = snapshot.metrics.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names.len(), pmt_obs::metrics::CATALOG.len());
    assert!(names.contains(&"prometheus.otel.dropped"));
}

/// A client of `kind` for `profile` (call inside a runtime).
async fn client_of(
    home: &Path,
    kind: c::ClientKind,
) -> (pmt_rpc::client::Client, pmt_rpc::client::Inbox) {
    let mut hello = daemon::hello(false);
    hello.client = kind;
    pmt_rpc::client::Client::connect(
        &record(home).endpoint,
        pmt_rpc::control::Purpose::Interactive,
        vec!["default".into()],
        &hello,
    )
    .await
    .unwrap()
}

/// The first `usage.updated` payload `inbox` receives.
async fn usage_updated(inbox: &mut pmt_rpc::client::Inbox) -> serde_json::Value {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let event = inbox.events.recv().await.expect("an event");
            if event.event == "usage.updated" {
                return event.data;
            }
        }
    })
    .await
    .expect("a usage.updated event")
}

/// Scenario: TUI and desktop agree.
#[test]
fn every_attached_client_receives_the_same_usage_update() {
    let home = mock_home();
    append_config(home.path(), "\n[budgets]\ndaily_usd = 5.0\n");
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (tui, desktop) = runtime.block_on(async {
        let (tui, mut tui_inbox) = client_of(home.path(), c::ClientKind::Tui).await;
        let (desktop, mut desktop_inbox) = client_of(home.path(), c::ClientKind::Desktop).await;
        let created = tui
            .call("session.create", serde_json::json!({}))
            .await
            .unwrap();
        let id = created["session_id"].as_str().unwrap().to_owned();
        desktop
            .call("session.attach", serde_json::json!({"session_id": id}))
            .await
            .unwrap();
        tui.call(
            "prompt.submit",
            serde_json::json!({"session_id": id, "text": "hi"}),
        )
        .await
        .unwrap();
        (
            usage_updated(&mut tui_inbox).await,
            usage_updated(&mut desktop_inbox).await,
        )
    });
    assert_eq!(tui, desktop);
    assert_eq!(tui["session_totals"]["calls"], 1, "{tui}");
    assert_eq!(tui["root_totals"]["calls"], 1);
    assert_eq!(tui["call"]["purpose"], "main");
    assert!(tui["call"].get("request_hash").is_none(), "{tui}");
    let budget = &tui["budgets"][0];
    assert_eq!(
        (budget["scope"].as_str(), budget["limit"].as_f64()),
        (Some("daily"), Some(5.0))
    );
}

/// Scenario: Store migration in progress.
#[test]
fn readyz_names_the_store_while_it_opens_and_turns_ready() {
    let home = mock_home();
    let mut cmd = common::command(home.path());
    cmd.env("PMT_TEST_STORE_DELAY_MS", "4000")
        .args(["daemon", "run"]);
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().unwrap();
    let health = wait_for_health_url(home.path());
    let (status, body) = get_url(&health, "/readyz");
    assert_eq!(status, 503, "{body}");
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["status"], "not_ready");
    assert!(
        body["failing"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "store"),
        "{body}"
    );
    assert!(
        !body.to_string().contains("default"),
        "no profile names: {body}"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let (status, body) = get_url(&health, "/readyz");
        if status == 200 {
            assert_eq!(body, r#"{"status":"ok"}"#);
            break;
        }
        assert!(std::time::Instant::now() < deadline, "never ready: {body}");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = common::run(home.path(), &["daemon", "stop"]);
    let _ = child.wait();
}

/// The health listener's URL, from the log record of a daemon whose RPC
/// endpoint is not published yet (its stores still open).
fn wait_for_health_url(home: &Path) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let log = std::fs::read_to_string(home.join("logs/gateway.log")).unwrap_or_default();
        if let Some(url) = log
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find_map(|r| r["fields"]["health_url"].as_str().map(str::to_owned))
        {
            return url;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no health listener logged: {log}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn get_url(url: &str, path: &str) -> (u16, String) {
    let addr = url.trim_start_matches("http://");
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    (status, body)
}

/// Scenario: Scripted health check.
#[test]
fn status_check_exits_1_naming_a_down_adapter() {
    let home = mock_home();
    let adapters = home.path().join("adapters");
    std::fs::create_dir_all(adapters.join("default")).unwrap();
    std::fs::write(adapters.join("default/fatal"), "").unwrap();
    append_config(
        home.path(),
        "\n[gateway.platforms.test]\nenabled = true\nallow_all = true\n",
    );
    let mut cmd = common::command(home.path());
    cmd.env("PMT_TEST_ADAPTER_DIR", &adapters)
        .args(["daemon", "start"]);
    assert!(cmd.output().unwrap().status.success());
    let _stop = Stop(home.path());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let out = loop {
        let out = common::run(home.path(), &["status", "--check"]);
        if common::stdout(&out).contains("adapter:test") {
            break out;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{}",
            common::stdout(&out)
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let text = common::stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("down     adapter:test (default)"), "{text}");
    // Without --check the same report exits 0.
    assert_eq!(common::run(home.path(), &["status"]).status.code(), Some(0));
}

struct Stop<'a>(&'a Path);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = common::run(self.0, &["daemon", "stop"]);
    }
}

#[test]
fn status_without_a_daemon_says_so_and_check_exits_1() {
    let home = mock_home();
    let out = common::run(home.path(), &["status"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        common::stdout(&out).starts_with("daemon: not running\n"),
        "{}",
        common::stdout(&out)
    );
    assert_eq!(
        common::run(home.path(), &["status", "--check"])
            .status
            .code(),
        Some(1)
    );
    let out = common::run(home.path(), &["status", "--liveness"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(common::stdout(&out).is_empty());
}

#[test]
fn liveness_prints_nothing_and_exits_0_for_a_live_daemon() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let out = common::run(home.path(), &["status", "--liveness"]);
    assert_eq!(out.status.code(), Some(0), "{}", common::stderr(&out));
    assert!(common::stdout(&out).is_empty() && common::stderr(&out).is_empty());
}

#[test]
fn system_health_returns_the_components_of_the_connections_profile() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let health = runtime.block_on(async {
        let (client, _inbox) = connect(home.path(), "default").await;
        client
            .call_typed::<c::SystemHealthCall>(&c::SystemParams::default())
            .await
            .unwrap()
    });
    let named = |name: &str| {
        health
            .components
            .iter()
            .find(|c| c.component == name)
            .cloned()
    };
    let store = named("store").expect("the store component");
    assert_eq!(
        (store.state.as_str(), store.profile.as_deref()),
        ("ok", Some("default"))
    );
    assert_eq!(named("scheduler").map(|c| c.state), Some("ok".to_owned()));
}

/// Scenario: Slash command in a chat (`/insights 7` from a TUI client).
#[test]
fn insights_in_a_session_shows_the_report_and_adds_no_ledger_row() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let ledger_rows = || {
        let out = common::run(home.path(), &["--json", "usage", "--by", "session"]);
        let body: serde_json::Value = serde_json::from_str(&common::stdout(&out)).unwrap();
        body["total"]["calls"].as_u64().unwrap()
    };
    let (tui, mut inbox) = runtime.block_on(client_of(home.path(), c::ClientKind::Tui));
    let id = runtime.block_on(async {
        let created = tui
            .call("session.create", serde_json::json!({}))
            .await
            .unwrap();
        let id = created["session_id"].as_str().unwrap().to_owned();
        tui.call(
            "prompt.submit",
            serde_json::json!({"session_id": id, "text": "hi"}),
        )
        .await
        .unwrap();
        while let Some(event) = inbox.events.recv().await {
            if event.event == "turn.finished" {
                break;
            }
        }
        id
    });
    let before = ledger_rows();
    let reply = runtime
        .block_on(tui.call(
            "command.dispatch",
            serde_json::json!({"session_id": id, "name": "insights", "args": "7"}),
        ))
        .unwrap();
    let text = reply.to_string();
    assert!(text.contains("Insights for the last 7 days"), "{reply}");
    assert!(text.contains("turns 1"), "{reply}");
    assert_eq!(ledger_rows(), before, "the report added a ledger row");
    assert_eq!(before, 1);
}
