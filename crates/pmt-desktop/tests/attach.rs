//! Attaching through the bundled binary (spec: desktop-app, Daemon
//! attachment through the bundled binary; Ensure is bounded; Local
//! endpoint validation; Version skew restarts a stale daemon), against a
//! fake `prometheus` script and a fake control channel.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pmt_desktop::{Attach, Native, Shell, ShellConfig};
use serde_json::json;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

mod common;
use common::Silent;

/// A fake `prometheus`: `version` prints `identity`, `daemon ensure`
/// prints the contents of `ensure.out` (after sleeping `ensure.sleep`
/// seconds), and `daemon restart` is recorded in `calls`.
fn fake_binary(dir: &Path, identity: &str, ensure: &str, exit: i32, sleep: u32) -> PathBuf {
    std::fs::write(dir.join("ensure.out"), ensure).unwrap();
    let script = format!(
        r#"#!/bin/sh
dir="$(dirname "$0")"
echo "$*" >> "$dir/calls"
case "$*" in
  *version*) echo '{identity}' ;;
  *restart*) exit 0 ;;
  *ensure*) sleep {sleep}; cat "$dir/ensure.out"; echo "last words" >&2; exit {exit} ;;
esac
"#
    );
    let path = dir.join("prometheus");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn shell(binary: PathBuf, home: &Path, bound: Duration) -> Shell {
    Shell::new(
        ShellConfig {
            binary,
            home: home.to_path_buf(),
            user_home: home.to_path_buf(),
            ensure_bound: bound,
        },
        Arc::new(Silent) as Arc<dyn Native>,
    )
}

fn ensure_json(rpc_url: &str, control: &str, version: &str, build: &str) -> String {
    json!({
        "rpc_url": rpc_url,
        "control": control,
        "instance_id": "inst",
        "build_id": build,
        "version": version,
        "rpc_protocol": "1.0",
        "features": ["session-attach"],
    })
    .to_string()
}

fn calls(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("calls")).unwrap_or_default()
}

#[tokio::test]
async fn ensure_printing_garbage_is_a_no_result_error_and_no_attachment() {
    let dir = tempfile::tempdir().unwrap();
    let identity = r#"{"version":"1.0.0","build_id":"b"}"#;
    let binary = fake_binary(dir.path(), identity, "starting the daemon...\n", 2, 0);
    let outcome = shell(binary, dir.path(), Duration::from_secs(20))
        .attach(None, false)
        .await;
    assert_eq!(
        outcome,
        Attach::Failed {
            error: "prometheus daemon ensure produced no result (exit 2): last words".into(),
            log: None
        }
    );
}

#[tokio::test]
async fn a_cold_start_past_the_deadline_is_a_timeout_with_the_log_link() {
    let dir = tempfile::tempdir().unwrap();
    let identity = r#"{"version":"1.0.0","build_id":"b"}"#;
    let ok = ensure_json("ws://127.0.0.1:4000/rpc", "/nowhere", "1.0.0", "b");
    let binary = fake_binary(dir.path(), identity, &ok, 0, 30);
    let started = std::time::Instant::now();
    let outcome = shell(binary, dir.path(), Duration::from_secs(1))
        .attach(None, false)
        .await;
    assert!(started.elapsed() < Duration::from_secs(10));
    let log = dir.path().join("logs").join("gateway.log");
    assert_eq!(
        outcome,
        Attach::Failed {
            error: "gateway daemon did not become ready within 1 s".into(),
            log: Some(log.to_string_lossy().into_owned())
        }
    );
}

#[tokio::test]
async fn a_non_loopback_endpoint_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let identity = r#"{"version":"1.0.0","build_id":"b"}"#;
    let remote = ensure_json("http://192.168.1.5:9120", "/nowhere", "1.0.0", "b");
    let binary = fake_binary(dir.path(), identity, &remote, 0, 0);
    let outcome = shell(binary, dir.path(), Duration::from_secs(20))
        .attach(None, false)
        .await;
    assert_eq!(
        outcome,
        Attach::Failed {
            error: "Invalid local gateway endpoint: not loopback".into(),
            log: None
        }
    );
}

#[tokio::test]
async fn a_daemon_that_outlived_a_cli_update_to_an_older_app_keeps_running() {
    let dir = tempfile::tempdir().unwrap();
    let control = fake_control(dir.path(), 0);
    let identity = r#"{"version":"1.5.2","build_id":"old"}"#;
    let newer = ensure_json("ws://127.0.0.1:4000/rpc", &control, "1.6.0", "new");
    let binary = fake_binary(dir.path(), identity, &newer, 0, 0);
    let outcome = shell(binary, dir.path(), Duration::from_secs(20))
        .attach(None, false)
        .await;
    assert_eq!(
        outcome,
        Attach::Attached {
            version: "1.6.0".into(),
            profiles: vec!["default".into()],
            notice: Some(
                "Prometheus 1.6.0 is running but this app bundles 1.5.2; update the app".into()
            )
        }
    );
    assert!(
        !calls(dir.path()).contains("restart"),
        "{}",
        calls(dir.path())
    );
}

#[tokio::test]
async fn a_stale_daemon_restarts_silently_but_asks_first_while_turns_run() {
    let dir = tempfile::tempdir().unwrap();
    let busy = fake_control(dir.path(), 1);
    let identity = r#"{"version":"1.6.0","build_id":"app"}"#;
    let stale = ensure_json("ws://127.0.0.1:4000/rpc", &busy, "1.5.0", "old");
    let binary = fake_binary(dir.path(), identity, &stale, 0, 0);
    let shell = shell(binary, dir.path(), Duration::from_secs(20));
    let asked = shell.attach(None, false).await;
    assert_eq!(
        asked,
        Attach::ConfirmRestart {
            running_turns: 1,
            daemon: "1.5.0".into(),
            bundled: "1.6.0".into()
        }
    );
    assert!(!calls(dir.path()).contains("restart"));
    let confirmed = shell.attach(None, true).await;
    assert!(
        matches!(confirmed, Attach::Attached { .. }),
        "{confirmed:?}"
    );
    assert!(calls(dir.path()).contains("daemon restart"));
}

#[tokio::test]
async fn a_plugin_refusal_is_logged_at_warn_with_the_plugin_id() {
    let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = {
        let logs = Arc::clone(&logs);
        move || common::Capture(Arc::clone(&logs))
    };
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let dir = tempfile::tempdir().unwrap();
    let shell = shell(
        dir.path().join("prometheus"),
        dir.path(),
        Duration::from_secs(1),
    );
    let request = serde_json::from_value(json!({
        "command": "plugin_refused",
        "args": {"plugin": "a", "command": "reveal_in_folder"}
    }))
    .unwrap();
    let answer = shell.handle("main", request).await.unwrap();
    assert_eq!(answer, json!({"ok": false, "reason": "plugin"}));
    let text = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert!(text.contains("WARN"), "{text}");
    assert!(text.contains("plugin=\"a\""), "{text}");
}

/// A control channel answering `status` with `active_turns` running.
fn fake_control(dir: &Path, active_turns: usize) -> String {
    let path = dir.join(format!("control-{active_turns}.sock"));
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    pmt_core::spawn::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            let _ = BufReader::new(read).read_line(&mut line).await;
            let answer = json!({"status": {
                "pid": 1, "uptime_secs": 1, "version": "x", "build_id": "x",
                "instance_id": "inst", "rpc_url": "ws://127.0.0.1:4000/rpc",
                "rpc_protocol": "1.0", "profiles": ["default"], "connections": 0,
                "sessions": 0, "active_turns": active_turns, "retiring": false
            }});
            let _ = write.write_all(format!("{answer}\n").as_bytes()).await;
        }
    });
    path.to_string_lossy().into_owned()
}
