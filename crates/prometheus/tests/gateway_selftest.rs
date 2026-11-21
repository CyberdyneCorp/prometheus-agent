//! `prometheus gateway selftest` (spec: platform-adapters, Peer-agent
//! routing self-test): offline, without credentials, each failure named
//! with its category; exit 0 only when every check passes.

#![cfg(feature = "gateway")]

mod common;

use serde_json::Value;

use self::common::{mock_home, run};

fn selftest(home: &std::path::Path, args: &[&str]) -> (Option<i32>, Value) {
    let mut all = vec!["--json", "gateway", "selftest"];
    all.extend_from_slice(args);
    let out = run(home, &all);
    let report = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    (out.status.code(), report)
}

fn failures(report: &Value) -> Vec<Value> {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["ok"] == false)
        .cloned()
        .collect()
}

/// Scenario: Bot mentions ignored by config.
#[test]
fn bot_mentions_ignored_by_config_are_a_config_failure() {
    let home = mock_home();
    common::append_config(
        home.path(),
        "\n[gateway.platforms.slack]\nenabled = true\nallow_users = [\"U0ANA\"]\n",
    );
    let (code, report) = selftest(home.path(), &["slack", "--peer-agent"]);
    assert_eq!(code, Some(1), "{report:#}");
    let failed = failures(&report);
    assert_eq!(failed.len(), 1, "{report:#}");
    assert_eq!(failed[0]["category"], "config");
    assert!(
        failed[0]["detail"].as_str().unwrap().contains("allow_bots"),
        "{failed:?}"
    );
    // Without --peer-agent the person paths pass.
    let (code, report) = selftest(home.path(), &["slack"]);
    assert_eq!(code, Some(0), "{report:#}");
}

#[test]
fn an_allowed_peer_passes_and_a_disabled_platform_fails() {
    let home = mock_home();
    common::append_config(
        home.path(),
        "\n[gateway.platforms.slack]\nenabled = true\nallow_users = [\"U0ANA\", \"B0PEER\"]\n\
         [gateway.bots]\nallow_bots = \"mentions\"\n",
    );
    let (code, report) = selftest(
        home.path(),
        &["slack", "--peer-agent", "--peer-id", "B0PEER"],
    );
    assert_eq!(code, Some(0), "{report:#}");
    let (code, report) = selftest(home.path(), &["discord"]);
    assert_eq!(code, Some(1));
    assert_eq!(failures(&report)[0]["category"], "config");
}
