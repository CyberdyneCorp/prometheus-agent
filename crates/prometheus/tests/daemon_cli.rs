//! `prometheus daemon` and `prometheus rpc` against real daemon processes
//! on temporary homes (spec: messaging-gateway, One gateway daemon per
//! home; workspace-architecture, One daemon per Prometheus home; Daemon
//! discovery file and local control channel; JSON-RPC framing and error
//! codes; Catalog index of capability-owned methods and events; cli,
//! Process identity from runtime records).

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;

use pmt_rpc::client::Client;
use pmt_rpc::contracts::{ClientCapabilities, ClientKind, HelloParams, Version};
use pmt_rpc::control::Purpose;
use pmt_rpc::discovery::{Discovery, discover};
use serde_json::{Value, json};

use self::common::{append_config, mock_home, ok, run, stderr, stdout, temp_home};

/// Stops the home's daemon when the test ends, whatever happened.
struct Stopper<'a>(&'a Path);

impl Drop for Stopper<'_> {
    fn drop(&mut self) {
        let _ = run(self.0, &["daemon", "stop"]);
    }
}

fn start(home: &Path) -> (Stopper<'_>, u32) {
    let out = run(home, &["daemon", "start"]);
    let text = ok(&out);
    assert!(text.starts_with("daemon started (pid "), "{text}");
    (Stopper(home), daemon_pid(home))
}

fn daemon_pid(home: &Path) -> u32 {
    match discover(home).unwrap() {
        Discovery::Running(record) => record.process.pid,
        other => panic!("no daemon: {other:?}"),
    }
}

#[test]
fn a_second_daemon_exits_75_without_opening_any_store() {
    let home = mock_home();
    let (_stop, pid) = start(home.path());
    ok(&run(home.path(), &["profile", "create", "extra"]));
    let out = run(home.path(), &["daemon", "run"]);
    assert_eq!(out.status.code(), Some(75), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!(
            "gateway already running (pid {pid}); attach with 'prometheus tui' or stop it with 'prometheus daemon stop'"
        )),
        "{}",
        stderr(&out)
    );
    assert!(
        !home.path().join("profiles/extra/state.db").exists(),
        "the refused daemon opened no store"
    );
}

#[test]
fn starting_a_running_daemon_is_a_no_op() {
    let home = mock_home();
    let (_stop, pid) = start(home.path());
    let out = run(home.path(), &["daemon", "start"]);
    assert_eq!(ok(&out), format!("gateway already running (pid {pid})\n"));
    assert_eq!(daemon_pid(home.path()), pid, "no second daemon");
}

#[test]
fn a_stale_endpoint_file_is_replaced_by_the_new_daemon() {
    let home = mock_home();
    let run_dir = home.path().join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let stale = json!({
        "pid": dead_pid(), "start_time": 1, "version": "0.0.1", "role": "daemon", "profiles": [],
        "instance_id": "old", "build_id": "old", "rpc_url": "ws://127.0.0.1:9/rpc",
        "control": "nowhere", "rpc_protocol": "1.0", "started_at": "2020-01-01T00:00:00Z"
    });
    std::fs::write(run_dir.join("gateway.json"), stale.to_string()).unwrap();
    let (_stop, pid) = start(home.path());
    let record: Value =
        serde_json::from_slice(&std::fs::read(run_dir.join("gateway.json")).unwrap()).unwrap();
    assert_eq!(record["pid"], pid);
    assert_ne!(record["instance_id"], "old");
}

/// The pid of a process that has exited.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new(common::BIN)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[test]
fn status_reports_a_reused_pid_record_as_not_running_and_removes_it() {
    let home = temp_home();
    let run_dir = home.path().join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    // This test process is alive, but started at another time.
    let record = json!({
        "pid": std::process::id(), "start_time": 1, "version": "0.0.1", "role": "daemon", "profiles": []
    });
    std::fs::write(run_dir.join("gateway.json"), record.to_string()).unwrap();
    let out = run(home.path(), &["daemon", "status"]);
    assert_eq!(ok(&out), "gateway is not running\n");
    assert!(!run_dir.join("gateway.json").exists());
}

#[test]
fn ensure_prints_the_endpoint_status_and_stop_end_it() {
    let home = mock_home();
    let out = run(home.path(), &["--json", "daemon", "ensure"]);
    let _stop = Stopper(home.path());
    let endpoint: Value = serde_json::from_str(&ok(&out)).unwrap();
    let mut keys: Vec<&str> = endpoint
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "build_id",
            "control",
            "features",
            "instance_id",
            "rpc_protocol",
            "rpc_url",
            "version"
        ]
    );
    assert_eq!(endpoint["features"], json!(["session-attach"]));
    assert_eq!(endpoint["version"], env!("CARGO_PKG_VERSION"));
    let again: Value =
        serde_json::from_str(&ok(&run(home.path(), &["--json", "daemon", "ensure"]))).unwrap();
    assert_eq!(again, endpoint, "the same daemon");

    let status: Value =
        serde_json::from_str(&ok(&run(home.path(), &["--json", "daemon", "status"]))).unwrap();
    assert_eq!(status["schema"], "prometheus.daemon_status.v1");
    assert_eq!(status["running"], true);
    assert_eq!(status["daemon"]["rpc_url"], endpoint["rpc_url"]);
    assert_eq!(status["daemon"]["profiles"], json!(["default"]));

    assert_eq!(
        ok(&run(home.path(), &["daemon", "stop"])),
        "daemon stopped\n"
    );
    assert!(!home.path().join("run/gateway.json").exists());
    assert_eq!(
        ok(&run(home.path(), &["daemon", "status"])),
        "gateway is not running\n"
    );
}

#[test]
fn an_invalid_profile_config_exits_78_naming_the_key() {
    let home = mock_home();
    append_config(home.path(), "\n[busy]\ninput_mode = \"loud\"\n");
    let out = run(home.path(), &["daemon", "run"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(stderr(&out).contains("busy.input_mode"), "{}", stderr(&out));
}

#[test]
fn rpc_schema_records_each_name_with_its_owner() {
    let home = temp_home();
    let schema: Value = serde_json::from_str(&ok(&run(home.path(), &["rpc", "schema"]))).unwrap();
    assert_eq!(schema["methods"]["worktree.new"]["owner"], "git-worktrees");
    assert_eq!(schema["methods"]["journey.list"]["owner"], "memory");
    assert_eq!(
        schema["events"]["automation.state"]["owner"],
        "session-automation"
    );
    let checked_in = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../schema/rpc.json"
    ))
    .unwrap();
    assert_eq!(stdout(&run(home.path(), &["rpc", "schema"])), checked_in);
}

fn hello() -> HelloParams {
    HelloParams {
        client: ClientKind::Cli,
        client_version: env!("CARGO_PKG_VERSION").into(),
        protocol: Version { major: 1, minor: 0 },
        capabilities: ClientCapabilities::default(),
    }
}

#[test]
fn a_spent_session_budget_refuses_prompt_submit_with_32005() {
    let home = mock_home();
    ok(&run(home.path(), &["chat", "-z", "spend some tokens"]));
    let session = common::newest_session(home.path());
    ok(&run(
        home.path(),
        &["config", "set", "budgets.session_tokens", "1"],
    ));
    let (_stop, _) = start(home.path());
    let Discovery::Running(record) = discover(home.path()).unwrap() else {
        panic!("no daemon");
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let err = runtime.block_on(async {
        let (client, _inbox) = Client::connect(
            &record.endpoint,
            Purpose::Interactive,
            vec!["default".into()],
            &hello(),
        )
        .await
        .unwrap();
        client
            .call("session.resume", json!({"session_id": session}))
            .await
            .unwrap();
        client
            .call(
                "prompt.submit",
                json!({"session_id": session, "text": "more"}),
            )
            .await
            .unwrap_err()
    });
    assert_eq!(err.code, -32_005, "{err}");
    assert_eq!(err.kind(), Some("budget_exceeded"));
}

#[test]
fn a_session_runs_its_turn_in_the_daemon() {
    let home = mock_home();
    let (_stop, _) = start(home.path());
    let Discovery::Running(record) = discover(home.path()).unwrap() else {
        panic!("no daemon");
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let finished = runtime.block_on(async {
        let (client, mut inbox) = Client::connect(
            &record.endpoint,
            Purpose::Interactive,
            vec!["default".into()],
            &hello(),
        )
        .await
        .unwrap();
        let created = client.call("session.create", json!({})).await.unwrap();
        let session = created["session_id"].as_str().unwrap().to_owned();
        client
            .call(
                "prompt.submit",
                json!({"session_id": session, "text": "hello daemon"}),
            )
            .await
            .unwrap();
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(30), inbox.events.recv())
                    .await
                    .unwrap()
                    .unwrap();
            if event.event == "turn.finished" {
                break event;
            }
        }
    });
    assert_eq!(finished.data["final_response"], "mock: hello daemon");
    assert_eq!(finished.data["exit_reason"], "completed");
}

#[test]
fn a_daemon_that_fails_to_start_reports_its_error() {
    let home = mock_home();
    std::fs::write(
        home.path().join("daemon.toml"),
        "[daemon.rpc]\nbind = \"0.0.0.0:7777\"\n",
    )
    .unwrap();
    let out = run(home.path(), &["daemon", "start"]);
    assert_eq!(out.status.code(), Some(69), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("the daemon exited with exit status: 78")
            && stderr(&out).contains("the client listener is loopback-only"),
        "{}",
        stderr(&out)
    );
}

/// Scenario: doctor reports the running daemon (workspace-architecture,
/// Doctor: `daemon` running, build and protocol match). Regression: the
/// control status call used to start a nested runtime and panic.
#[test]
fn doctor_reports_a_running_daemon_of_this_build() {
    let home = mock_home();
    let _daemon = start(home.path());
    let out = run(home.path(), &["doctor", "daemon"]);
    let text = ok(&out);
    assert!(text.contains("daemon running (pid "), "{text}");
    let all = run(home.path(), &["doctor"]);
    assert_ne!(all.status.code(), Some(70), "{}", stderr(&all));
    assert!(
        stdout(&all).contains("daemon running (pid "),
        "{}",
        stdout(&all)
    );
}

/// A daemon of another build (same version) is not reported as current.
#[test]
fn doctor_warns_about_a_daemon_of_another_build() {
    let home = mock_home();
    let _daemon = start(home.path());
    let path = home.path().join("run/gateway.json");
    // The daemon's first heartbeat rewrites the record right after it is
    // published; the edit goes in after that beat (the next one is a
    // minute away), or the beat would put the real build id back.
    let read =
        || -> Value { serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap() };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while read().get("updated_at").is_none() {
        assert!(std::time::Instant::now() < deadline, "no heartbeat");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut record = read();
    record["build_id"] = json!("another-build");
    std::fs::write(&path, record.to_string()).unwrap();
    let text = stdout(&run(home.path(), &["doctor", "daemon"]));
    assert!(
        text.contains("warn daemon        running another build"),
        "{text}"
    );
    assert!(text.contains("build another-build"), "{text}");
}

/// Regression: a foreground `daemon run` in a shell that inherited a
/// service manager's markers (a CI runner or GNOME Terminal under systemd:
/// `INVOCATION_ID`; an app launchd started: `XPC_SERVICE_NAME`) took itself
/// for supervised, so a restart or update handover waited for a supervisor
/// that never comes. Only a daemon the manager itself started counts.
#[test]
fn a_foreground_daemon_under_an_inherited_service_marker_is_unsupervised() {
    let home = mock_home();
    let mut first = common::command(home.path())
        .args(["daemon", "run"])
        .env("INVOCATION_ID", "0123456789abcdef")
        .env("XPC_SERVICE_NAME", "application.com.example.runner")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let _stopper = Stopper(home.path());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !matches!(discover(home.path()), Ok(Discovery::Running(_))) {
        assert!(std::time::Instant::now() < deadline, "the daemon never ran");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let status: Value =
        serde_json::from_str(&ok(&run(home.path(), &["--json", "daemon", "status"]))).unwrap();
    assert_eq!(status["daemon"]["supervised"], false, "{status}");
    let restarted = common::command(home.path())
        .args(["daemon", "restart"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    ok(&restarted);
    assert!(first.wait().is_ok());
    assert_ne!(daemon_pid(home.path()), first.id());
}

/// Regression: a daemon `daemon start` launched from inside a systemd
/// service (a CI runner: `INVOCATION_ID` set) took itself for supervised,
/// so `daemon restart` asked it to exit 75 and waited for a supervisor
/// that never restarts it. It is unsupervised and restarts in place.
#[test]
fn a_daemon_started_from_a_systemd_service_restarts_itself() {
    let home = mock_home();
    let out = common::command(home.path())
        .args(["daemon", "start"])
        .env("INVOCATION_ID", "0123456789abcdef")
        .env("XPC_SERVICE_NAME", "application.com.example.runner")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(ok(&out).starts_with("daemon started (pid "));
    let _stopper = Stopper(home.path());
    let first = daemon_pid(home.path());
    let status: Value =
        serde_json::from_str(&ok(&run(home.path(), &["--json", "daemon", "status"]))).unwrap();
    assert_eq!(status["daemon"]["supervised"], false, "{status}");
    let restarted = common::command(home.path())
        .args(["daemon", "restart"])
        .env("INVOCATION_ID", "0123456789abcdef")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    ok(&restarted);
    assert_ne!(daemon_pid(home.path()), first);
}

/// A container-init-like supervisor: runs the multiplexing daemon of
/// `home` marked supervised and starts it again whenever it exits 75.
fn supervise(home: &Path) -> std::thread::JoinHandle<()> {
    let home = home.to_owned();
    std::thread::spawn(move || {
        loop {
            let status = common::command(&home)
                .args(["--set", "gateway.multiplex=true", "daemon", "run"])
                .env("PMT_SUPERVISOR", "test")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            if status.code() != Some(75) {
                return;
            }
        }
    })
}

/// A supervised `daemon restart` returns once the new daemon answers, so a
/// `daemon status` straight after it already lists a profile created
/// before the restart (the container image's in-place restart check).
#[test]
fn a_status_right_after_a_supervised_restart_lists_a_new_profile() {
    let home = mock_home();
    let supervisor = supervise(home.path());
    let stopper = Stopper(home.path());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !matches!(discover(home.path()), Ok(Discovery::Running(_))) {
        assert!(std::time::Instant::now() < deadline, "the daemon never ran");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    ok(&run(home.path(), &["profile", "create", "work"]));
    let restarted = ok(&run(home.path(), &["daemon", "restart"]));
    assert!(
        restarted.starts_with("daemon restarted in place by its supervisor"),
        "{restarted}"
    );
    let status: Value =
        serde_json::from_str(&ok(&run(home.path(), &["--json", "daemon", "status"]))).unwrap();
    assert_eq!(status["daemon"]["supervised"], true, "{status}");
    let profiles = status["daemon"]["profiles"].as_array().unwrap();
    assert!(profiles.iter().any(|p| p == "work"), "{status}");
    drop(stopper);
    supervisor.join().unwrap();
}
