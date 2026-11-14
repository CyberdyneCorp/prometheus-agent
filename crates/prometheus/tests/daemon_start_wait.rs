//! `daemon start` (and every command that ensures the daemon, such as
//! `dashboard`) waits for a daemon that is still starting instead of
//! spawning a second one, which could only fail on the home lock (spec:
//! messaging-gateway, One gateway daemon per home). On a loaded machine a daemon
//! can take more than a few seconds between taking the home and
//! publishing its endpoint.

mod common;

use std::process::Stdio;
use std::time::Duration;

use pmt_core::runtime::{Role, RuntimeRecord, WriterLock, run_dir};
use pmt_rpc::discovery::{DaemonRecord, Endpoint};

/// Where the stand-in daemon says it listens (nothing dials it).
fn endpoint() -> Endpoint {
    Endpoint {
        instance_id: "starting".into(),
        build_id: "b".into(),
        rpc_url: "ws://127.0.0.1:9/rpc".into(),
        control: "unused".into(),
        rpc_protocol: "1.0".into(),
        started_at: "2026-11-14T00:00:00Z".into(),
        health_url: None,
        features: Vec::new(),
    }
}

#[test]
fn a_start_waits_for_a_daemon_that_is_still_starting() {
    let home = common::mock_home();
    // A live process that has taken the home but not published where to
    // reach it yet (this test process stands in for the starting daemon).
    let process = RuntimeRecord::current(Role::Daemon, vec!["default".into()]);
    let record = run_dir(home.path()).join(Role::Daemon.record_file());
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    std::fs::write(&record, serde_json::to_vec(&process).unwrap()).unwrap();
    let start = common::command(home.path())
        .args(["daemon", "start"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Longer than the old 5 s patience, well within a start's own bound.
    std::thread::sleep(Duration::from_secs(7));
    let published = DaemonRecord {
        process: process.clone(),
        endpoint: endpoint(),
    };
    std::fs::write(&record, serde_json::to_vec(&published).unwrap()).unwrap();
    let out = start.wait_with_output().unwrap();
    let _ = std::fs::remove_file(&record);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout.trim(),
        format!("gateway already running (pid {})", process.pid),
        "the starting daemon is the one used; none was spawned"
    );
}

#[test]
fn a_start_that_loses_the_home_lock_waits_for_the_holder_to_publish() {
    let home = common::mock_home();
    // Another daemon took the home lock and has not written its record
    // yet: our start's own daemon can only fail on the lock (exit 75).
    let lock = WriterLock::acquire(home.path(), Role::Daemon, vec!["default".into()]).unwrap();
    let record = run_dir(home.path()).join(Role::Daemon.record_file());
    std::fs::remove_file(&record).unwrap();
    let start = common::command(home.path())
        .args(["daemon", "start"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(3));
    lock.publish(&endpoint()).unwrap();
    let out = start.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout.trim(),
        format!("gateway already running (pid {})", lock.record().pid)
    );
}
