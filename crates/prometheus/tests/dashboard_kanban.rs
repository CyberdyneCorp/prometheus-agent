//! The kanban dashboard API (spec: web-dashboard, Kanban dashboard API and
//! orchestration modes): orchestration settings with configured and
//! effective values, unknown profiles refused with nothing written, the
//! read endpoints, and the dashboard's authentication, CSRF and observer
//! rules on every kanban route. The browser side (Terminate, the
//! Auto/Manual control) is `dashboard_browser.rs`.

#![cfg(feature = "dashboard")]

mod common;

use serde_json::{Value, json};

use common::api::Request;
use common::dashboard::{self, call, get, sign_in};
use common::{daemon, mock_home, ok, run};

fn config_text(home: &std::path::Path) -> String {
    std::fs::read_to_string(home.join("profiles/default/config.toml")).unwrap()
}

/// Scenario: Unknown orchestrator profile.
#[test]
fn an_unknown_orchestrator_profile_is_refused_and_nothing_is_written() {
    let home = mock_home();
    ok(&run(home.path(), &["profile", "create", "planner"]));
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let signed = sign_in(&opened);
    let before = config_text(home.path());

    let refused = call(
        opened.port,
        "PUT",
        "/api/kanban/orchestration",
        &signed,
        Some(&json!({"orchestrator_profile": "ghost", "auto_decompose": true})),
    );
    assert_eq!(refused.status, 400, "{refused:?}");
    assert!(
        refused.json()["message"]
            .as_str()
            .is_some_and(|m| m.contains("ghost")),
        "{refused:?}"
    );
    assert_eq!(config_text(home.path()), before);

    let saved = call(
        opened.port,
        "PUT",
        "/api/kanban/orchestration",
        &signed,
        Some(&json!({"orchestrator_profile": "planner", "auto_decompose": true})),
    );
    assert_eq!(saved.status, 200, "{saved:?}");
    let saved = saved.json();
    assert_eq!(saved["configured"]["orchestrator_profile"], "planner");
    assert_eq!(saved["configured"]["auto_decompose"], true);
    assert_eq!(saved["configured"]["auto_promote_children"], Value::Null);
    assert_eq!(saved["effective"]["mode"], "auto");
    assert_eq!(saved["effective"]["auto_promote_children"], true);
    assert_eq!(saved["effective"]["orchestrator_profile"], "planner");
    let read = get(opened.port, "/api/kanban/orchestration", &signed.cookie).json();
    assert_eq!(read, saved);
    assert_eq!(
        ok(&run(
            home.path(),
            &["config", "get", "kanban.orchestrator_profile"]
        ))
        .trim(),
        "planner"
    );

    // A profile removed after it was configured shows as configured but
    // not effective, with the reason.
    let edited = config_text(home.path()).replace("\"planner\"", "\"retired\"");
    std::fs::write(home.path().join("profiles/default/config.toml"), edited).unwrap();
    let read = get(opened.port, "/api/kanban/orchestration", &signed.cookie).json();
    assert_eq!(read["configured"]["orchestrator_profile"], "retired");
    assert_eq!(read["effective"]["orchestrator_profile"], Value::Null);
    assert!(
        read["problems"][0].as_str().unwrap().contains("retired"),
        "{read}"
    );
}

/// The read endpoints answer the shapes the page renders, and an unknown
/// run is a 404.
#[test]
fn the_kanban_read_endpoints_describe_boards_workers_and_the_backlog() {
    let home = mock_home();
    let created = ok(&run(
        home.path(),
        &[
            "--json",
            "kanban",
            "create",
            "triage idea",
            "--status",
            "triage",
        ],
    ));
    let task = serde_json::from_str::<Value>(&created).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let signed = sign_in(&opened);

    let boards = get(opened.port, "/api/kanban/boards", &signed.cookie);
    assert_eq!(boards.status, 200, "{boards:?}");
    let boards = boards.json();
    assert_eq!(boards["boards"][0]["slug"], "default");
    assert_eq!(boards["boards"][0]["tasks"][0]["id"], task.as_str());
    assert_eq!(boards["boards"][0]["tasks"][0]["status"], "triage");

    let workers = get(opened.port, "/api/kanban/workers/active", &signed.cookie).json();
    assert_eq!(workers["count"], 0, "{workers}");

    let inspect = get(opened.port, "/api/kanban/inspect", &signed.cookie).json();
    assert_eq!(inspect["backlog"]["triage"], 1, "{inspect}");
    assert_eq!(inspect["in_progress"], 0, "{inspect}");
    assert!(
        inspect["in_progress_limit"].as_u64().unwrap() >= 1,
        "{inspect}"
    );
    assert!(
        inspect["recent_events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["task_id"] == task.as_str() && e["kind"] == "created"),
        "{inspect}"
    );

    let missing = get(opened.port, "/api/kanban/runs/r_nothing", &signed.cookie);
    assert_eq!(missing.status, 404, "{missing:?}");
    assert_eq!(missing.json()["error"], "kanban.run_not_found");
    let gone = call(
        opened.port,
        "POST",
        "/api/kanban/runs/r_nothing/terminate",
        &signed,
        Some(&json!({})),
    );
    assert_eq!(gone.status, 404, "{gone:?}");
}

/// Sends the head of a POST declaring a `length`-byte body, without the
/// body: a refusal by declared size answers before any byte is read.
fn declared_upload(
    port: u16,
    path: &str,
    signed: &dashboard::Signed,
    length: u64,
) -> common::api::Answer {
    use std::io::{Read as _, Write as _};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_mins(1)))
        .unwrap();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: {}\r\nX-Prometheus-CSRF: {}\r\n\
         Content-Type: application/pdf\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n",
        signed.cookie, signed.csrf
    );
    stream.write_all(head.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    common::api::parse(&raw)
}

/// Scenario: Oversized upload refused (the dashboard surface). A small
/// upload is stored and listed; a 26 MiB one is refused by its declared
/// length with the shared message, and nothing is stored.
#[test]
fn an_oversized_dashboard_upload_is_refused_and_nothing_is_stored() {
    let home = mock_home();
    let created = ok(&run(home.path(), &["--json", "kanban", "create", "report"]));
    let task = serde_json::from_str::<Value>(&created).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let signed = sign_in(&opened);
    let path = format!("/api/kanban/boards/default/tasks/{task}/attachments?name=notes.txt");

    let mut small = Request::new("POST", &path)
        .header("Cookie", &signed.cookie)
        .header("X-Prometheus-CSRF", &signed.csrf);
    small.body = Some("quarterly numbers".to_owned());
    let stored = small.send(opened.port);
    assert_eq!(stored.status, 200, "{stored:?}");
    assert_eq!(stored.json()["filename"], "notes.txt");
    assert_eq!(stored.json()["size"], 17);
    let listed = ok(&run(
        home.path(),
        &["--json", "kanban", "attachments", &task],
    ));
    let listed: Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(
        listed["attachments"].as_array().unwrap().len(),
        1,
        "{listed}"
    );
    let blobs = home.path().join("kanban/blobs");
    let before = common::files_under(&blobs).len();

    let size = 26 * 1024 * 1024;
    let refused = declared_upload(opened.port, &path, &signed, size);
    assert_eq!(refused.status, 413, "{refused:?}");
    assert_eq!(refused.json()["error"], "kanban.attachment_too_large");
    assert_eq!(
        refused.json()["message"],
        format!("attachment too large: {size} > {}", 25 * 1024 * 1024)
    );
    let listed = ok(&run(
        home.path(),
        &["--json", "kanban", "attachments", &task],
    ));
    let listed: Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(
        listed["attachments"].as_array().unwrap().len(),
        1,
        "{listed}"
    );
    assert_eq!(common::files_under(&blobs).len(), before, "no blob stored");
}

/// Every kanban route is behind the dashboard's sign-in, a cookie change
/// needs the CSRF token, and an observer device may read but not change.
#[test]
fn kanban_routes_follow_the_dashboard_auth_rules() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let signed = sign_in(&opened);

    let anonymous = Request::new("GET", "/api/kanban/boards").send(opened.port);
    assert_eq!(anonymous.status, 401, "{anonymous:?}");
    let no_csrf = Request::new("PUT", "/api/kanban/orchestration")
        .header("Cookie", &signed.cookie)
        .json(&json!({"auto_decompose": true}))
        .send(opened.port);
    assert_eq!(no_csrf.status, 403, "{no_csrf:?}");
    assert_eq!(no_csrf.json()["error"], "csrf");

    let paired = ok(&run(
        home.path(),
        &[
            "--json",
            "daemon",
            "pair-device",
            "viewer",
            "--role",
            "observer",
        ],
    ));
    let token = serde_json::from_str::<Value>(&paired).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let bearer = format!("Bearer {token}");
    let read = Request::new("GET", "/api/kanban/inspect")
        .header("Authorization", &bearer)
        .send(opened.port);
    assert_eq!(read.status, 200, "{read:?}");
    for (method, path) in [
        ("PUT", "/api/kanban/orchestration"),
        ("POST", "/api/kanban/runs/r_nothing/terminate"),
        ("POST", "/api/kanban/boards/default/tasks/t_x/attachments"),
    ] {
        let refused = Request::new(method, path)
            .header("Authorization", &bearer)
            .json(&json!({"auto_decompose": true}))
            .send(opened.port);
        assert_eq!(refused.status, 403, "{method} {path}: {refused:?}");
        assert_eq!(refused.json()["error"], "observer");
    }
    assert!(!config_text(home.path()).contains("auto_decompose"));
}
