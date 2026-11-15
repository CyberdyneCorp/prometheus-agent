//! Kanban inside the real daemon (spec: kanban, Single dispatcher per
//! home; Notifications through the owning profile; /kanban slash command;
//! Subscription delivery modes and inheritance), delivering through the
//! debug-build file adapter.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use self::common::gateway::{
    Daemon, adapter_dir, gateway_home, gateway_home_with, send, texts, wait, wait_said,
};
use self::common::{ok, run, run_with_stdin, stderr};

const RULES: &str = "[[rule]]\nrepeat = true\ntext = \"hi\"\n";

fn json(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend(args);
    serde_json::from_str(&ok(&run(home, &all))).unwrap()
}

fn daemon_holds_the_lease(home: &Path) {
    wait(
        "the daemon takes the dispatcher lease",
        Duration::from_mins(1),
        || {
            json(home, &["kanban", "diag"])["lease"]["holder"]
                .as_str()
                .is_some_and(|h| h.starts_with("daemon"))
        },
    );
}

/// Scenario: Standalone loop cannot race the daemon.
#[test]
fn a_standalone_loop_exits_75_while_the_daemon_dispatches() {
    let home = gateway_home_with(RULES, "\n[kanban]\ndispatch_interval_secs = 1\n");
    let daemon = Daemon::start(home.path());
    daemon_holds_the_lease(home.path());
    let out = run(home.path(), &["kanban", "dispatch", "--loop"]);
    assert_eq!(out.status.code(), Some(75), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("dispatcher lease held by daemon"),
        "{}",
        stderr(&out)
    );
    daemon.stop(home.path());
    // A stopped daemon gives its lease back: one tick runs at once.
    ok(&run(home.path(), &["kanban", "dispatch", "--once"]));
}

/// Scenario: Multi-profile notification.
#[test]
fn a_notification_goes_out_through_the_subscribers_own_adapter() {
    let home = gateway_home_with(RULES, "\n[gateway]\nmultiplex = true\n");
    ok(&run(
        home.path(),
        &["profile", "create", "work", "--no-alias"],
    ));
    for (key, value) in [
        ("gateway.platforms.test.enabled", "true"),
        ("gateway.platforms.test.allow_all", "true"),
        ("model.provider", "mock"),
        ("model.default", "mock"),
    ] {
        ok(&run(
            home.path(),
            &["-p", "work", "config", "set", key, value],
        ));
    }
    let daemon = Daemon::start(home.path());
    let id = json(home.path(), &["kanban", "create", "ship it"])["id"]
        .as_str()
        .unwrap()
        .to_owned();
    ok(&run(
        home.path(),
        &[
            "-p",
            "work",
            "kanban",
            "notify",
            "subscribe",
            &id,
            "--adapter",
            "test",
            "--chat",
            "chat-9",
        ],
    ));
    // A route the default profile cannot reach is skipped, never borrowed.
    ok(&run(
        home.path(),
        &[
            "kanban",
            "notify",
            "subscribe",
            &id,
            "--adapter",
            "telegram",
            "--chat",
            "42",
        ],
    ));
    ok(&run(
        home.path(),
        &["kanban", "complete", &id, "--result", "shipped v2"],
    ));
    wait(
        "work's adapter reports the completion",
        Duration::from_mins(1),
        || {
            texts(home.path(), "work")
                .iter()
                .any(|t| t.contains("completed: shipped v2"))
        },
    );
    let sent: Vec<Value> = common::gateway::sent(home.path(), "work")
        .into_iter()
        .filter(|m| m["kind"] == "send")
        .collect();
    assert_eq!(sent[0]["chat"], "chat-9", "{sent:?}");
    assert!(
        texts(home.path(), "default")
            .iter()
            .all(|t| !t.contains("shipped v2"))
    );
    daemon.stop(home.path());
}

fn create(home: &Path, args: &[&str]) -> String {
    let mut all = vec!["kanban", "create"];
    all.extend(args);
    json(home, &all)["id"].as_str().unwrap().to_owned()
}

fn status(home: &Path, id: &str) -> String {
    json(home, &["kanban", "show", id])["task"]["status"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Scenario: Unblock from a phone mid-turn.
#[test]
fn kanban_unblock_runs_mid_turn_without_disturbing_it() {
    let home = gateway_home("\n[kanban]\ndispatch = false\n");
    let id = create(home.path(), &["stuck card"]);
    ok(&run(
        home.path(),
        &[
            "kanban",
            "block",
            &id,
            "--kind",
            "needs_input",
            "--reason",
            "which region?",
        ],
    ));
    let daemon = Daemon::start(home.path());
    send(home.path(), "default", "u1", "slow");
    common::wait_for_turn_start(home.path());
    send(
        home.path(),
        "default",
        "u1",
        &format!("/kanban unblock {id}"),
    );
    wait_said(home.path(), "default", &format!("ok {id} ready"));
    assert_eq!(status(home.path(), &id), "ready");
    let said = texts(home.path(), "default");
    assert!(
        said.iter()
            .all(|t| !t.contains("slow done") && !t.to_lowercase().contains("interrupt")),
        "the running turn continues undisturbed: {said:?}"
    );
    daemon.kill();
}

/// Scenario: Created card reports back.
#[test]
fn a_card_created_from_a_chat_reports_back_and_wakes_it() {
    let rules = "[[rule]]\nuser_regex = 'kanban notification'\nrepeat = true\ntext = \"woken: on it\"\n\n\
        [[rule]]\nrepeat = true\ntext = \"hi\"\n";
    let home = gateway_home_with(rules, "\n[kanban]\ndispatch = false\n");
    let daemon = Daemon::start(home.path());
    send(
        home.path(),
        "default",
        "u1",
        "/kanban create \"transcribe podcast\"",
    );
    wait_said(home.path(), "default", "is subscribed (notify+wake)");
    let id = json(home.path(), &["kanban", "list"])["tasks"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let subs = json(home.path(), &["kanban", "notify", "list", &id]);
    assert_eq!(subs["subscriptions"][0]["delivery_mode"], "notify+wake");
    assert_eq!(subs["subscriptions"][0]["chat"], "u1");
    ok(&run(
        home.path(),
        &["kanban", "complete", &id, "--result", "transcribed"],
    ));
    wait_said(home.path(), "default", "completed: transcribed");
    wait_said(home.path(), "default", "woken: on it");
    daemon.kill();
}

/// Drops a message of `sender` into a thread of `chat`.
fn send_thread(home: &Path, chat: &str, thread: &str, text: &str) {
    let inbox = adapter_dir(home).join("default").join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let message = serde_json::json!({"id": format!("t{n}"), "sender": "u7", "chat": chat,
        "chat_type": "thread", "thread": thread, "text": text, "mentioned": true});
    let tmp = inbox.join(format!("t{n}.tmp"));
    std::fs::write(&tmp, message.to_string()).unwrap();
    std::fs::rename(&tmp, inbox.join(format!("t{n}.json"))).unwrap();
}

/// Scenario: Review feedback wakes the originating chat.
#[test]
fn review_feedback_wakes_the_subscribed_thread() {
    let rules = "[[rule]]\nuser_regex = 'kanban notification'\nrepeat = true\ntext = \"woken for the review\"\n\n\
        [[rule]]\nrepeat = true\ntext = \"hi\"\n";
    let home = gateway_home_with(rules, "\n[kanban]\ndispatch = false\n");
    let daemon = Daemon::start(home.path());
    send_thread(home.path(), "c1", "t9", "hello thread");
    wait_said(home.path(), "default", "hi");
    let id = create(home.path(), &["fix the login page"]);
    ok(&run(
        home.path(),
        &[
            "kanban",
            "notify",
            "subscribe",
            &id,
            "--adapter",
            "test",
            "--chat",
            "c1",
            "--thread",
            "t9",
            "--delivery-mode",
            "notify+wake",
            "--chat-type",
            "thread",
        ],
    ));
    ok(&run(
        home.path(),
        &["kanban", "request-review", &id, "--summary", "first try"],
    ));
    wait_said(home.path(), "default", "first try");
    ok(&run(
        home.path(),
        &[
            "kanban",
            "request-changes",
            &id,
            "--comment",
            "the button is misaligned",
        ],
    ));
    ok(&run(home.path(), &["kanban", "promote", &id]));
    let ticket = json(home.path(), &["kanban", "claim", &id])["ticket"]
        .as_str()
        .unwrap()
        .to_owned();
    ok(&run_with_stdin(
        home.path(),
        &[
            "kanban",
            "request-review",
            &id,
            "--ticket",
            "-",
            "--summary",
            "fixed the feedback",
        ],
        &format!("{ticket}\n"),
    ));
    wait_said(
        home.path(),
        "default",
        "is ready for review: fixed the feedback",
    );
    wait(
        "the thread's agent takes a turn per review request",
        Duration::from_mins(1),
        || {
            texts(home.path(), "default")
                .iter()
                .filter(|t| t.contains("woken for the review"))
                .count()
                >= 2
        },
    );
    let sent: Vec<Value> = common::gateway::sent(home.path(), "default")
        .into_iter()
        .filter(|m| m["kind"] == "send" && m["text"].as_str().is_some_and(|t| t.contains("woken")))
        .collect();
    assert!(sent.iter().all(|m| m["chat"] == "c1"), "{sent:?}");
    let session = common::newest_session(home.path());
    let messages = common::session_messages(home.path(), &session);
    assert!(
        messages
            .iter()
            .any(|m| m.to_string().contains("fixed the feedback")),
        "the wake turn carries the summary"
    );
    daemon.kill();
}
