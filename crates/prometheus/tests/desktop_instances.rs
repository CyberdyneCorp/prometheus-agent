//! The desktop across connected instances, with two real daemons (spec:
//! desktop-app, Agents across connections; Update every connected
//! instance): the union roster with disambiguated handles, a mention that
//! stays on its own machine, and Update all instances with one offline.
#![cfg(all(unix, feature = "dashboard"))]

mod common;

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use common::connections::{self, ask, ask0, call, close, dial};
use common::release::{Fixture, next_patch, sign};
use common::{daemon, dashboard, mock_home, ok, run, temp_home};
use pmt_desktop::connections::ssh::SshPrograms;
use pmt_desktop::update::{FEED_SCHEMA, Fetch, Fetching, Updater, UpdaterConfig};
use pmt_update::Channel;
use serde_json::{Value, json};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

/// A home serving `default` and a `research` profile on the mock provider.
fn machine() -> tempfile::TempDir {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "gateway.multiplex", "true"],
    ));
    ok(&run(home.path(), &["profile", "create", "research"]));
    for (key, value) in [("model.provider", "mock"), ("model.default", "mock")] {
        ok(&run(
            home.path(),
            &["-p", "research", "config", "set", key, value],
        ));
    }
    home
}

/// A device credential of `home`'s daemon with `role`.
fn device(run_it: impl Fn(&[&str]) -> std::process::Output, role: &str) -> String {
    let printed = ok(&run_it(&[
        "--json",
        "daemon",
        "pair-device",
        "desktop",
        "--role",
        role,
    ]));
    serde_json::from_str::<Value>(&printed).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Registers the dashboard of a running daemon as remote connection
/// `name` with `token`; its id.
async fn add_remote(shell: &pmt_desktop::Shell, name: &str, port: u16, token: &str) -> String {
    let saved = ask(
        shell,
        "connection_save",
        json!({"name": name, "kind": "remote", "url": format!("http://127.0.0.1:{port}")}),
    )
    .await
    .unwrap();
    let id = saved["id"].as_str().unwrap().to_owned();
    ask(shell, "connection_pair", json!({"id": id, "token": token}))
        .await
        .unwrap();
    id
}

/// A port nothing listens on.
fn closed_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Scenario: Same profile name on two machines; Scenario: Mention does
/// not cross machines.
#[test]
fn agents_of_two_machines_share_a_roster_but_mentions_stay_local() {
    let laptop = machine();
    let homelab = machine();
    let _laptop_daemon = daemon::start(laptop.path(), &[]);
    let _homelab_daemon = daemon::start(homelab.path(), &[]);
    let port = dashboard::open(homelab.path(), &[]).port;
    let token = device(|args| run(homelab.path(), args), "driver");
    let app = laptop.path().join("app");
    let (shell, _keychain) = connections::shell(laptop.path(), &app, SshPrograms::default());
    runtime().block_on(async {
        ask(&shell, "connection_save", json!({"id": "local", "name": "Laptop", "kind": "local"}))
            .await
            .unwrap();
        add_remote(&shell, "Homelab", port, &token).await;
        ask(&shell, "connection_save", json!({"name": "VPS", "kind": "remote", "url": format!("http://127.0.0.1:{}", closed_port())}))
            .await
            .unwrap();
        let roster = ask0(&shell, "agents_roster").await.unwrap();
        let handles: Vec<&str> = roster["agents"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a["handle"].as_str())
            .collect();
        assert!(handles.contains(&"@research-homelab"), "{roster}");
        assert!(handles.contains(&"@research-laptop"), "{roster}");
        assert!(handles.contains(&"@default-laptop"), "{roster}");
        let vps = roster["connections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "VPS")
            .unwrap();
        assert_eq!(vps["reachable"], false, "{roster}");
        assert!(vps["error"].as_str().unwrap().contains("unreachable"), "{roster}");

        // The Laptop window submits the mention on its own connection.
        ask(&shell, "attach", json!({"profile": "research"})).await.unwrap();
        let answer = ask(&shell, "prepare_dial", json!({"profile": "research"}))
            .await
            .unwrap();
        let mut ws = dial(&answer).await.unwrap();
        let created = call(&mut ws, 1, "session.create", json!({"profile": "research", "source": "desktop"})).await;
        let session = created["result"]["session_id"].as_str().unwrap().to_owned();
        let submitted = call(&mut ws, 2, "prompt.submit", json!({"session_id": session, "text": "@research-homelab what do you think?"})).await;
        assert!(submitted.get("result").is_some(), "{submitted}");
        finished(&mut ws, &session).await;
        close(ws).await;
    });
    let sessions = |home: &std::path::Path| {
        let listed = ok(&run(
            home,
            &["-p", "research", "--json", "sessions", "list", "--all"],
        ));
        serde_json::from_str::<Value>(&listed).unwrap()["sessions"]
            .as_array()
            .unwrap()
            .len()
    };
    assert_eq!(sessions(laptop.path()), 1, "the turn ran on the Laptop");
    assert_eq!(sessions(homelab.path()), 0, "no turn started on Homelab");
}

/// Reads events until `session`'s turn finished.
async fn finished(ws: &mut connections::Socket, session: &str) {
    use futures_util::StreamExt as _;
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(30), ws.next())
            .await
            .expect("the turn finishes within 30 s");
        let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) = frame else {
            panic!("socket ended: {frame:?}");
        };
        let event: Value = serde_json::from_str(text.as_str()).unwrap();
        if event["params"]["event"] == "turn.finished" && event["params"]["session_id"] == session {
            return;
        }
    }
}

/// The app's release origin in memory, signed with the test key.
#[derive(Default)]
struct Origin(Mutex<BTreeMap<String, Vec<u8>>>);

impl Origin {
    fn publish(&self, url: &str, bytes: &[u8]) {
        let mut files = self.0.lock().unwrap();
        files.insert(format!("{url}.minisig"), sign(bytes).into_bytes());
        files.insert(url.to_owned(), bytes.to_vec());
    }
}

impl Fetch for Origin {
    fn get<'a>(&'a self, url: &'a str) -> Fetching<'a> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .ok_or_else(|| format!("404 {url}"))
        })
    }
}

/// The app at 1.0.0 with 1.1.0 on its channel.
fn app_updater(dir: &std::path::Path) -> Updater {
    const ORIGIN: &str = "https://releases.example";
    let origin = Origin::default();
    let feed = json!({
        "schema": FEED_SCHEMA, "version": "1.1.0", "notes": "",
        "platforms": {"test-platform": pmt_desktop::update::Archive::of("desktop.tar.gz", b"app archive")},
    });
    origin.publish(
        &format!("{ORIGIN}/channels/desktop-stable.json"),
        feed.to_string().as_bytes(),
    );
    origin.publish(&format!("{ORIGIN}/v1.1.0/desktop.tar.gz"), b"app archive");
    Updater::new(
        UpdaterConfig {
            release_key: Some(include_str!("../../pmt-update/keys/test.pub").to_owned()),
            channel: "stable".into(),
            origin: ORIGIN.into(),
            platform: "test-platform".into(),
            current: "1.0.0".into(),
            dir: dir.to_path_buf(),
        },
        Box::new(origin),
    )
}

/// Scenario: One instance is unreachable.
#[test]
fn update_all_updates_the_reachable_instances_and_marks_the_offline_one() {
    let homelab = Fixture::self_install();
    ok(&homelab.run(&["config", "set", "model.provider", "mock"]));
    let next = next_patch(homelab.version);
    homelab.publish(&next, Channel::Stable, &homelab.bytes(), None);
    ok(&homelab.run(&["daemon", "start"]));
    let shown = ok(&homelab.run(&["--json", "dashboard", "--no-open", "--port", "0"]));
    let port = dashboard::parse(&shown).port;
    let token = device(|args| homelab.run(args), "admin");
    let laptop = temp_home();
    let app = laptop.path().join("app");
    let (shell, _keychain) = connections::shell(laptop.path(), &app, SshPrograms::default());
    let shell = shell.with_updater(app_updater(&app.join("updates")));
    let rows = runtime().block_on(async {
        add_remote(&shell, "Homelab", port, &token).await;
        ask(&shell, "connection_save", json!({"name": "VPS", "kind": "remote", "url": format!("http://127.0.0.1:{}", closed_port())}))
            .await
            .unwrap();
        ask0(&shell, "update_all").await.unwrap()
    });
    let row = |name: &str| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no row {name}: {rows}"))
    };
    assert_eq!(row("Homelab")["outcome"], "started", "{rows}");
    assert_eq!(row("VPS")["outcome"], "unreachable", "{rows}");
    assert_eq!(row("This app")["outcome"], "staged", "{rows}");
    assert_eq!(
        rows.as_array().unwrap().last().unwrap()["name"],
        "This app",
        "the app updates last"
    );
    // The remote instance really updates: its updater hands the daemon
    // over to the new build and records a successful receipt.
    let deadline = std::time::Instant::now() + Duration::from_mins(6);
    loop {
        let receipt = std::fs::read_to_string(homelab.home.join("updates/receipts/latest.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok());
        if receipt.as_ref().is_some_and(|r| r["outcome"] == "success") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the update finished: {receipt:?}"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    let _ = homelab.run(&["daemon", "stop"]);
}

/// An instance its package manager updates reports the owner's refusal on
/// its row; a driver device may not update a daemon at all.
#[test]
fn an_owned_instance_reports_its_owner_and_drivers_may_not_update() {
    let brewed = Fixture::new(Some("homebrew"));
    ok(&brewed.run(&["config", "set", "model.provider", "mock"]));
    ok(&brewed.run(&["daemon", "start"]));
    let shown = ok(&brewed.run(&["--json", "dashboard", "--no-open", "--port", "0"]));
    let port = dashboard::parse(&shown).port;
    let admin = device(|args| brewed.run(args), "admin");
    let laptop = temp_home();
    let app = laptop.path().join("app");
    let (shell, _keychain) = connections::shell(laptop.path(), &app, SshPrograms::default());
    let rows = runtime().block_on(async {
        add_remote(&shell, "Brewed", port, &admin).await;
        ask0(&shell, "update_all").await.unwrap()
    });
    let brewed_row = &rows[0];
    assert_eq!(brewed_row["outcome"], "refused", "{rows}");
    assert!(
        brewed_row["message"]
            .as_str()
            .unwrap()
            .contains("owner: homebrew"),
        "{rows}"
    );
    assert_eq!(
        rows[1]["outcome"], "unavailable",
        "a commit build's app has no updater: {rows}"
    );

    let driver = device(|args| brewed.run(args), "driver");
    let laptop2 = temp_home();
    let (shell, _keychain) = connections::shell(
        laptop2.path(),
        &laptop2.path().join("app"),
        SshPrograms::default(),
    );
    let rows = runtime().block_on(async {
        add_remote(&shell, "Brewed", port, &driver).await;
        ask0(&shell, "update_all").await.unwrap()
    });
    assert_eq!(rows[0]["outcome"], "refused", "{rows}");
    assert!(
        rows[0]["message"].as_str().unwrap().contains("admin"),
        "{rows}"
    );
    let _ = brewed.run(&["daemon", "stop"]);
}
