//! The self-termination guard in a daemon-hosted session (spec:
//! tool-system, Self-termination guard): under yolo the model's commands
//! that would stop the daemon are blocked and the daemon keeps running.
//!
//! The commands only send signal 0, so a guard that failed would not stop
//! anything on the machine; the test still sees it from the tool result.

#![cfg(all(feature = "gateway", unix))]

mod common;

use std::path::Path;
use std::time::Duration;

use pmt_rpc::client::Client;
use pmt_rpc::control::Purpose;
use serde_json::json;

use self::common::{daemon, mock_home, ok, run};

fn script(pid: u32) -> String {
    format!(
        r#"[[rule]]
step = 0
[[rule.tool_calls]]
name = "terminal"
arguments = {{ command = "pkill -0 -f prometheus" }}

[[rule]]
step = 1
tool_result_regex = "BLOCKED \\(self-termination\\)"
[[rule.tool_calls]]
name = "terminal"
arguments = {{ command = "kill -0 {pid}" }}

[[rule]]
step = 2
tool_result_regex = "BLOCKED \\(self-termination\\)"
text = "both were blocked"

[[rule]]
text = "a command was not blocked"
"#
    )
}

async fn turn(home: &Path, script: &str) -> String {
    let (client, mut inbox) = Client::connect(
        &daemon::record(home).endpoint,
        Purpose::Interactive,
        vec!["default".into()],
        &daemon::hello(false),
    )
    .await
    .unwrap();
    let setup = json!({"setup": {"mock_script": script, "yolo": true}});
    let created = client.call("session.create", setup).await.unwrap();
    let session = created["session_id"].as_str().unwrap().to_owned();
    client
        .call(
            "prompt.submit",
            json!({"session_id": session, "text": "stop the daemon"}),
        )
        .await
        .unwrap();
    while let Some(event) = inbox.events.recv().await {
        if event.event == "turn.finished" {
            return event.data["final_response"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
        }
    }
    panic!("the session closed before the turn finished");
}

/// Scenario: pkill under yolo.
#[test]
fn pkill_under_yolo_is_blocked_and_the_daemon_keeps_running() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let pid = daemon::record(home.path()).process.pid;
    let path = home.path().join("s.toml");
    std::fs::write(&path, script(pid)).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let answer = runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_mins(1),
            turn(home.path(), &path.to_string_lossy()),
        )
        .await
        .expect("the turn finished")
    });
    assert_eq!(answer, "both were blocked");
    let status = ok(&run(home.path(), &["--json", "daemon", "status"]));
    assert!(status.contains(&pid.to_string()), "{status}");
}
