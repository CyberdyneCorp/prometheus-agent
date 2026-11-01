//! The daemon's served profile set and live configuration reload (spec:
//! configuration-and-profiles, Served profile set; Live reload with apply
//! classes).

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use pmt_rpc::client::{Client, Inbox};
use serde_json::{Value, json};

use self::common::{append_config, daemon, mock_home, ok, run};

fn status(home: &Path) -> Value {
    serde_json::from_str(&ok(&run(home, &["--json", "daemon", "status"]))).unwrap()
}

/// Polls `daemon status` until `done` holds for the default profile's
/// config state (at most 10 s).
fn wait_config(home: &Path, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = status(home)["daemon"]["config"][0].clone();
        if done(&state) {
            return state;
        }
        assert!(Instant::now() < deadline, "config state: {state}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

async fn turn(client: &Client, inbox: &mut Inbox, session: &str, text: &str) {
    client
        .call(
            "prompt.submit",
            json!({"session_id": session, "text": text}),
        )
        .await
        .unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), inbox.events.recv())
            .await
            .unwrap()
            .unwrap();
        if event.event == "turn.finished" && event.session_id.as_deref() == Some(session) {
            return;
        }
    }
}

async fn new_session(client: &Client) -> String {
    let created = client.call("session.create", json!({})).await.unwrap();
    created["session_id"].as_str().unwrap().to_owned()
}

/// The frozen system prompt text of `session`, read from the store.
fn prompt_of(home: &Path, session: &str) -> String {
    let db = home.join("profiles/default/state.db");
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    conn.query_row(
        "SELECT tiers FROM session_prompts WHERE session_id = ?1",
        [session],
        |row| row.get(0),
    )
    .unwrap()
}

/// Scenario: Prompt-affecting key deferred.
#[test]
fn a_next_session_key_leaves_the_live_session_and_reaches_new_ones() {
    let home = mock_home();
    std::fs::write(
        home.path().join("profiles/default/SOUL.md"),
        "I am Zorblax, the identity under test.\n",
    )
    .unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (client, mut inbox) = runtime.block_on(daemon::connect(home.path(), "default"));
    let live = runtime.block_on(async {
        let live = new_session(&client).await;
        turn(&client, &mut inbox, &live, "before").await;
        live
    });
    ok(&run(
        home.path(),
        &["config", "set", "prompt.load_identity", "false"],
    ));
    wait_config(home.path(), |c| c["reloads"] == 1);
    let next = runtime.block_on(async {
        turn(&client, &mut inbox, &live, "after").await;
        let next = new_session(&client).await;
        turn(&client, &mut inbox, &next, "fresh").await;
        next
    });
    assert!(prompt_of(home.path(), &live).contains("Zorblax"));
    assert!(!prompt_of(home.path(), &next).contains("Zorblax"));
}

/// Scenario: Invalid edit keeps the daemon running.
#[test]
fn an_invalid_edit_is_reported_and_the_previous_configuration_serves() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let config = home.path().join("profiles/default/config.toml");
    let good = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{good}\n[agent\nbroken = ")).unwrap();
    let state = wait_config(home.path(), |c| c["error"].is_string());
    assert!(
        state["error"].as_str().unwrap().contains("config.toml"),
        "{state}"
    );
    let text = ok(&run(home.path(), &["daemon", "status"]));
    assert!(
        text.contains("config error (default)")
            && text.contains("serving the previous configuration"),
        "{text}"
    );
    let doctor = common::stdout(&run(home.path(), &["doctor"]));
    assert!(doctor.contains("unclosed table"), "{doctor}");
    // Still serving, with the previous configuration.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let (client, mut inbox) = daemon::connect(home.path(), "default").await;
        let session = new_session(&client).await;
        turn(&client, &mut inbox, &session, "still here").await;
    });
    std::fs::write(&config, good).unwrap();
    wait_config(home.path(), |c| c.get("error").is_none());
}

#[test]
fn a_restart_key_waits_for_the_restart() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    append_config(home.path(), "\n[logging]\nlevel = \"debug\"\n");
    let state = wait_config(home.path(), |c| c["reloads"] == 1);
    assert_eq!(state["pending_restart"], json!(["logging.level"]));
    let text = ok(&run(home.path(), &["daemon", "status"]));
    assert!(
        text.contains("restart pending (default): 1 change(s) apply after 'prometheus daemon restart': logging.level"),
        "{text}"
    );
}

#[test]
fn the_launch_profile_alone_is_served_by_default() {
    let home = mock_home();
    ok(&run(home.path(), &["profile", "create", "work"]));
    let first = daemon::start(home.path(), &[]);
    assert_eq!(
        status(home.path())["daemon"]["profiles"],
        json!(["default"])
    );
    drop(first);
    let _daemon = daemon::start(home.path(), &["-p", "work"]);
    assert_eq!(status(home.path())["daemon"]["profiles"], json!(["work"]));
}

/// Scenario: Standalone profile excluded.
#[test]
fn multiplexing_serves_every_profile_except_standalone_ones() {
    let home = mock_home();
    ok(&run(home.path(), &["profile", "create", "work"]));
    ok(&run(home.path(), &["profile", "create", "lab"]));
    ok(&run(
        home.path(),
        &["-p", "lab", "config", "set", "gateway.standalone", "true"],
    ));
    ok(&run(
        home.path(),
        &["config", "set", "gateway.multiplex", "true"],
    ));
    let _daemon = daemon::start(home.path(), &[]);
    assert_eq!(
        status(home.path())["daemon"]["profiles"],
        json!(["default", "work"])
    );
    assert!(!home.path().join("profiles/lab/state.db").exists());
}
