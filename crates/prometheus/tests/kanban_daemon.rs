//! Kanban inside the real daemon (spec: kanban, Single dispatcher per
//! home; Notifications through the owning profile), delivering through
//! the debug-build file adapter.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use self::common::gateway::{Daemon, gateway_home_with, texts, wait};
use self::common::{ok, run, stderr};

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
