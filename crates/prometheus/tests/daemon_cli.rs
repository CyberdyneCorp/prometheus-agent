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
            "instance_id",
            "rpc_protocol",
            "rpc_url"
        ]
    );
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
