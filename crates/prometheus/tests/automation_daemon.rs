//! Session automation inside the real daemon: messaging conversations
//! through the debug-build file adapter (`PMT_TEST_ADAPTER_DIR`, platform
//! `test`) and client sessions over the client protocol (spec:
//! session-automation: Persisted per-session automation state; Goal
//! changes while a turn runs; Goal messages on messaging platforms;
//! Quality gates; Heartbeat ownership, recovery and silence on the
//! gateway; Automation status on every surface).

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use self::common::gateway::{
    Daemon, gateway_home, gateway_home_with, send, sent, texts, wait, wait_said,
};
use self::common::{daemon, ok, run};

fn db(home: &Path, profile: &str) -> rusqlite::Connection {
    rusqlite::Connection::open_with_flags(
        home.join("profiles").join(profile).join("state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
}

/// User turns of `profile` started by `origin`.
fn automated(home: &Path, profile: &str, origin: &str) -> i64 {
    db(home, profile)
        .query_row(
            "SELECT count(*) FROM messages WHERE role = 'user' \
             AND json_extract(metadata, '$.origin') = ?1",
            [origin],
            |r| r.get(0),
        )
        .unwrap()
}

/// Stored automation rows of `profile`.
fn rows(home: &Path, profile: &str) -> i64 {
    db(home, profile)
        .query_row("SELECT count(*) FROM automation", [], |r| r.get(0))
        .unwrap()
}

const JUDGE: &str = "You are the goal judge";

/// Scenario: Telegram continuation not quoted (and the status lines reach
/// the chat).
#[test]
fn goal_turns_in_a_chat_quote_nothing_and_say_their_status() {
    let rules = format!(
        "[[rule]]\nuser = \"{JUDGE}\"\ntimes = 1\ntext = '{{\"verdict\":\"continue\",\"reason\":\"3 files remain\"}}'\n\n\
         [[rule]]\nuser = \"{JUDGE}\"\nrepeat = true\ntext = '{{\"verdict\":\"done\",\"reason\":\"all ported\"}}'\n\n\
         [[rule]]\nrepeat = true\ntext = \"worked on it\"\n"
    );
    let home = gateway_home_with(&rules, "");
    let home = home.path();
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "/goal port the parser");
    wait_said(home, "default", "Goal achieved: all ported");
    let said = texts(home, "default");
    for line in [
        "Goal set: port the parser",
        "Continuing toward goal (1/20): 3 files remain",
    ] {
        assert!(said.iter().any(|t| t.contains(line)), "{said:?}");
    }
    let answers: Vec<Value> = sent(home, "default")
        .into_iter()
        .filter(|m| m["kind"] == "send" && m["text"] == "worked on it")
        .collect();
    assert_eq!(answers.len(), 2, "{answers:?}");
    assert!(
        answers.iter().all(|m| m["reply_to"].is_null()),
        "{answers:?}"
    );
    assert_eq!(automated(home, "default", "goal"), 2);
    daemon.stop(home);
}

/// Scenarios: Silent heartbeat; Reset clears automation.
#[test]
fn a_silent_heartbeat_sends_nothing_and_a_reset_drops_it() {
    let rules = "[[rule]]\nuser = \"[heartbeat]\"\nrepeat = true\ntext = \"NO_REPLY\"\n\n\
                 [[rule]]\nrepeat = true\ntext = \"fine\"\n";
    let home = gateway_home_with(rules, "");
    let home = home.path();
    common::append_config(home, "\n[heartbeat]\nmin_interval_secs = 1\n");
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "/heartbeat every 1s check CI");
    wait_said(home, "default", "Heartbeat set");
    wait("two heartbeats ran", Duration::from_mins(1), || {
        automated(home, "default", "heartbeat") >= 2
    });
    let said = texts(home, "default");
    assert!(!said.iter().any(|t| t.contains("NO_REPLY")), "{said:?}");
    assert_eq!(said.len(), 1, "only the command's answer: {said:?}");
    send(home, "default", "u1", "/new");
    wait("the heartbeat row is gone", Duration::from_secs(30), || {
        rows(home, "default") == 0
    });
    let after = automated(home, "default", "heartbeat");
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(automated(home, "default", "heartbeat"), after);
    daemon.stop(home);
}

/// Scenario: Restored after restart in its profile.
#[test]
fn a_chat_heartbeat_is_restored_after_a_restart_in_its_profile() {
    let home = gateway_home("");
    let home = home.path();
    ok(&run(home, &["profile", "create", "work"]));
    ok(&run(home, &["config", "set", "gateway.multiplex", "true"]));
    let work = home.join("profiles/work");
    std::fs::write(
        work.join("mock.toml"),
        "[[rule]]\nrepeat = true\ntext = \"work beat\"\n",
    )
    .unwrap();
    let mut config = std::fs::read_to_string(work.join("config.toml")).unwrap_or_default();
    config.push_str(
        "\n[model]\nprovider = \"mock\"\ndefault = \"mock\"\n\n[providers.mock]\nscript = \"mock.toml\"\n\n\
         [heartbeat]\nmin_interval_secs = 1\n\n[gateway.busy]\ndebounce_ms = 0\n\n\
         [gateway.platforms.test]\nenabled = true\nallow_all = true\n",
    );
    std::fs::write(work.join("config.toml"), config).unwrap();
    let beats = || {
        texts(home, "work")
            .iter()
            .filter(|t| t.contains("work beat"))
            .count()
    };
    let daemon = Daemon::start(home);
    send(home, "work", "u1", "/heartbeat every 2s anything new?");
    wait("a heartbeat", Duration::from_mins(1), || beats() >= 1);
    daemon.stop(home);
    let before = beats();
    let daemon = Daemon::start(home);
    wait(
        "a heartbeat after the restart",
        Duration::from_mins(1),
        || beats() > before,
    );
    daemon.stop(home);
    // Only the one command was ever sent: the restart restored it, under
    // `work` (the default profile has nothing).
    let human: i64 = db(home, "work")
        .query_row(
            "SELECT count(*) FROM messages WHERE role = 'user' \
             AND json_extract(metadata, '$.origin') IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(human, 0);
    assert_eq!(rows(home, "default"), 0);
}

/// Scenario: New goal rejected mid-run.
#[test]
fn a_new_goal_is_refused_while_the_chat_runs_a_turn() {
    let home = gateway_home("");
    let home = home.path();
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "slow");
    common::wait_for_turn_start(home);
    send(home, "default", "u1", "/goal write the report");
    wait_said(
        home,
        "default",
        "A turn is running; use /stop first, then set the new goal.",
    );
    assert_eq!(rows(home, "default"), 0);
    daemon.kill();
}

/// Quality gates from a chat: adding one needs a configured admin; the
/// list stays open to the session's users.
#[test]
fn adding_a_gate_from_a_chat_needs_a_configured_admin() {
    let home = gateway_home("");
    let home = home.path();
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "/goal gate add cargo test");
    wait_said(home, "default", "This command is restricted to admins.");
    send(home, "default", "u1", "/goal gate list");
    wait_said(home, "default", "No active goal.");
    daemon.stop(home);
    let home = gateway_home("admins = [\"boss\"]\n");
    let home = home.path();
    let daemon = Daemon::start(home);
    send(home, "default", "boss", "/goal ship it");
    wait_said(home, "default", "Goal set: ship it");
    send(home, "default", "boss", "/goal gate add true");
    wait_said(home, "default", "Quality gate 1 added: true");
    daemon.stop(home);
}

/// Scenario: Desktop closed, loop keeps firing (a client session's loop
/// runs in the daemon with no client attached).
#[test]
fn a_client_sessions_loop_keeps_firing_with_no_client_attached() {
    let home = common::mock_home();
    common::append_config(home.path(), "\n[loops]\nmin_interval_secs = 1\n");
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let id = runtime.block_on(async {
        let (client, mut inbox) = daemon::connect(home.path(), "default").await;
        let created = client.call("session.create", json!({})).await.unwrap();
        let id = created["session_id"].as_str().unwrap().to_owned();
        let set = client
            .call(
                "command.dispatch",
                json!({"session_id": id, "name": "loop", "args": "1s check the build"}),
            )
            .await
            .unwrap();
        assert!(
            set["notice"]
                .as_str()
                .unwrap_or_default()
                .starts_with("Loop set"),
            "{set}"
        );
        // Every surface hears of it.
        loop {
            let event = inbox.events.recv().await.unwrap();
            if event.event == "automation.state" && event.data["kind"] == "loop" {
                assert_eq!(event.data["status"], "active", "{:?}", event.data);
                assert!(
                    event.data["row"]
                        .as_str()
                        .unwrap()
                        .starts_with("loop · every 1s")
                );
                break;
            }
        }
        id
    });
    drop(runtime);
    wait("three wakeups ran", Duration::from_mins(1), || {
        automated(home.path(), "default", "loop") >= 3
    });
    let messages = common::session_messages(home.path(), &id);
    let replies = messages.iter().filter(|m| m["role"] == "assistant").count();
    assert!(replies >= 3, "{messages:?}");
}

/// Goal judge loop on a conversation's session when the turn comes from
/// an attached client, not the chat: the judge's continuation runs in the
/// chat once the session is idle (it is not left for the lane's next own
/// turn).
#[test]
fn a_client_turn_on_a_chats_goal_session_continues_in_the_chat() {
    let rules = format!(
        "[[rule]]\nuser = \"{JUDGE}\"\ntimes = 1\ntext = '{{\"verdict\":\"wait\",\"wait_for_seconds\":600,\"reason\":\"CI runs\"}}'\n\n\
         [[rule]]\nuser = \"{JUDGE}\"\ntimes = 1\ntext = '{{\"verdict\":\"continue\",\"reason\":\"1 file remains\"}}'\n\n\
         [[rule]]\nuser = \"{JUDGE}\"\nrepeat = true\ntext = '{{\"verdict\":\"done\",\"reason\":\"all ported\"}}'\n\n\
         [[rule]]\nrepeat = true\ntext = \"worked on it\"\n"
    );
    let home = gateway_home_with(&rules, "");
    let home = home.path();
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "/goal port the parser");
    wait_said(home, "default", "Goal parked on a timer");
    // Unparked without a new first turn: the goal drives the next turn's
    // judge, whoever submits that turn.
    send(home, "default", "u1", "/goal unwait");
    wait_said(home, "default", "Goal unparked.");
    let id = common::newest_session(home);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let (client, mut inbox) = daemon::connect(home, "default").await;
        client
            .call("session.attach", json!({"session_id": id}))
            .await
            .unwrap();
        client
            .call(
                "prompt.submit",
                json!({"session_id": id, "text": "how far along is it?"}),
            )
            .await
            .unwrap();
        while let Some(event) = inbox.events.recv().await {
            if event.event == "turn.finished" {
                break;
            }
        }
    });
    drop(runtime);
    wait_said(home, "default", "Goal achieved: all ported");
    let said = texts(home, "default");
    assert!(
        said.iter()
            .any(|t| t.contains("Continuing toward goal (1/20): 1 file remains")),
        "{said:?}"
    );
    // The first turn and the continuation; the client's turn was a person's.
    assert_eq!(automated(home, "default", "goal"), 2);
    daemon.stop(home);
}
