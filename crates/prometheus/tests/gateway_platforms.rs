//! Adapter operations outside a chat (spec: messaging-gateway, Adapter
//! operations from chat): `prometheus gateway pause|resume <platform>`
//! with and without the daemon, and `/platforms` on the line interface,
//! driven through the debug-build file adapter.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::Duration;

use self::common::gateway::{Daemon, adapter_dir, gateway_home, send, wait, wait_said};
use self::common::{ok, run, stdout};

fn admissions(home: &Path) -> Vec<String> {
    std::fs::read_to_string(adapter_dir(home).join("default").join("admissions.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .map(|v| v["admission"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn a_pause_set_without_the_daemon_holds_until_resumed_through_it() {
    let home = gateway_home("");
    let home = home.path();
    let out = ok(&run(home, &["gateway", "pause", "test"]));
    assert!(out.contains("the daemon is not running"), "{out}");
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "hello");
    wait(
        "the message is dropped as paused",
        Duration::from_secs(30),
        || admissions(home).contains(&"paused".to_owned()),
    );
    let status = ok(&run(home, &["gateway", "status"]));
    assert!(status.contains("[paused]"), "{status}");
    let out = ok(&run(home, &["gateway", "resume", "test"]));
    assert!(out.starts_with("Resumed. test: "), "{out}");
    send(home, "default", "u1", "hello again");
    wait_said(home, "default", "fast done");
    let json = stdout(&run(home, &["--json", "gateway", "pause", "test"]));
    let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(doc["paused"], true);
    assert_eq!(daemon.stop(home), Some(0));
}

#[test]
fn a_platform_the_profile_does_not_enable_is_refused() {
    let home = gateway_home("");
    let out = run(home.path(), &["gateway", "pause", "discord"]);
    assert_eq!(out.status.code(), Some(69));
    assert!(common::stderr(&out).contains("has no 'discord' adapter"));
}

#[cfg(unix)]
#[test]
fn platforms_on_the_line_interface_lists_the_daemons_adapters() {
    use expectrl::Expect;

    let home = gateway_home("");
    let home = home.path();
    let daemon = Daemon::start(home);
    let mut cmd = common::command(home);
    cmd.args(["--set", "ui.interface=line"])
        .current_dir(home)
        .env("TERM", "xterm");
    let mut p = expectrl::session::OsSession::spawn(cmd).unwrap();
    p.set_expect_timeout(Some(Duration::from_secs(30)));
    p.expect("› ").unwrap();
    p.send_line("/gateway").unwrap();
    p.expect("test: ").unwrap();
    p.send_line("/exit").unwrap();
    p.expect(expectrl::Eof).unwrap();
    daemon.stop(home);
}
