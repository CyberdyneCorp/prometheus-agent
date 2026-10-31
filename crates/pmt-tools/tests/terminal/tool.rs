//! The `terminal` and `process` tools through the dispatcher: a call runs
//! under the session's sandbox and returns the shaped result.

use std::sync::Arc;

use pmt_config::schema::Config;
use pmt_tools::environments::{Environments, SessionEnv};
use pmt_tools::result::Status;
use serde_json::{Value, json};

use crate::common::Fixture;

#[path = "../common/dispatcher.rs"]
mod dispatcher;

fn body(result: &pmt_tools::ToolResult) -> Value {
    serde_json::from_str(&result.text_content()).unwrap()
}

#[tokio::test]
async fn a_terminal_call_runs_under_the_sandbox_and_returns_shaped_output() {
    let fixture = Fixture::new();
    let environments = Arc::new(Environments::default());
    let _attached = environments.attach(
        "ses_1",
        SessionEnv {
            terminal: Some(Arc::new(fixture.session())),
            ..SessionEnv::default()
        },
    );
    let dispatcher = dispatcher::dispatcher(
        &environments,
        "ses_1",
        &Config::default(),
        &[],
        &fixture.profile,
    )
    .await;

    let ok = dispatcher::call(&dispatcher, "terminal", json!({"command": "echo hello"})).await;
    assert_eq!(ok.status, Status::Ok, "{}", ok.text_content());
    let reply = body(&ok);
    assert_eq!(
        (reply["output"].clone(), reply["exit_code"].clone()),
        (json!("hello\n"), json!(0))
    );

    // Outside the workspace the sandbox refuses the write; the command ran,
    // so the call is ok and the exit code tells the model.
    let target = fixture.outside.join("x");
    let refused = dispatcher::call(
        &dispatcher,
        "terminal",
        json!({"command": format!("touch {}", target.display())}),
    )
    .await;
    assert_eq!(refused.status, Status::Ok, "{}", refused.text_content());
    assert_ne!(body(&refused)["exit_code"], 0);
    assert!(!target.exists());

    // Long output is cut to its head and tail around one marker.
    let long = dispatcher::call(
        &dispatcher,
        "terminal",
        json!({"command": "i=0; while [ $i -lt 12000 ]; do echo 0123456789; i=$((i+1)); done"}),
    )
    .await;
    let output = body(&long)["output"].as_str().unwrap().to_owned();
    assert!(
        output.contains("[OUTPUT TRUNCATED - "),
        "{}",
        &output[..200]
    );
    assert!(output.chars().count() < 51_000);

    // A refusal before anything runs is an error result.
    let bad = dispatcher::call(
        &dispatcher,
        "terminal",
        json!({"command": "sleep 1", "pty": true}),
    )
    .await;
    assert_eq!(bad.status, Status::Error, "{}", bad.text_content());
}

#[tokio::test]
async fn background_processes_are_managed_through_the_process_tool() {
    let fixture = Fixture::new();
    let environments = Arc::new(Environments::default());
    let _attached = environments.attach(
        "ses_2",
        SessionEnv {
            terminal: Some(Arc::new(fixture.session())),
            ..SessionEnv::default()
        },
    );
    let dispatcher = dispatcher::dispatcher(
        &environments,
        "ses_2",
        &Config::default(),
        &[],
        &fixture.profile,
    )
    .await;
    let started = dispatcher::call(
        &dispatcher,
        "terminal",
        json!({"command": "echo started; sleep 30", "background": true}),
    )
    .await;
    let id = body(&started)["process_id"].as_str().unwrap().to_owned();
    let listed = dispatcher::call(&dispatcher, "process", json!({"action": "list"})).await;
    assert!(
        listed.text_content().contains(&id),
        "{}",
        listed.text_content()
    );
    let killed = dispatcher::call(
        &dispatcher,
        "process",
        json!({"action": "kill", "process_id": id}),
    )
    .await;
    assert_eq!(killed.status, Status::Ok, "{}", killed.text_content());
    let unknown = dispatcher::call(
        &dispatcher,
        "process",
        json!({"action": "poll", "process_id": "proc_nope"}),
    )
    .await;
    assert_eq!(unknown.status, Status::Error);
}
