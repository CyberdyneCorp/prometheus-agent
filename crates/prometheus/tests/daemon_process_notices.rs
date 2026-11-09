//! Background process notices of daemon-hosted interactive sessions (spec:
//! execution-environments, Quiet-wait auto-notify — Second unchanged poll):
//! a TUI or attached line chat is a client of the daemon, so a quiet-wait
//! switch there must reach its clients as a `process.notice` session
//! event; a one-shot session cannot receive one, so polling stays plain.

#![cfg(all(feature = "gateway", unix))] // the scripted command is POSIX sh

mod common;

use std::time::Duration;

use pmt_rpc::client::{Client, Inbox};
use serde_json::{Value, json};

use self::common::{daemon, mock_home};

/// A silent build in the background, polled twice, then the turn ends
/// saying whether the second poll promised a notification.
const SCRIPT: &str = r#"[[rule]]
step = 0
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "sleep 2; echo built", background = true }

[[rule]]
step = 1
[[rule.tool_calls]]
name = "process"
arguments = { action = "poll", process_id = "proc_" }

[[rule]]
step = 2
[[rule.tool_calls]]
name = "process"
arguments = { action = "poll", process_id = "proc_" }

[[rule]]
step = 3
tool_result_regex = "you will be notified when it exits"
text = "switched"

[[rule]]
step = 3
text = "plain"
"#;

/// Runs the script in a new session created with `extra` fields; returns
/// the turn's answer and every `process.notice` seen within `linger`
/// after the turn.
async fn poll_twice(
    client: &Client,
    inbox: &mut Inbox,
    script: &str,
    extra: Value,
    linger: Duration,
) -> (String, Vec<Value>) {
    let mut params = json!({"setup": {"mock_script": script, "yolo": true}});
    params
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let created = client.call("session.create", params).await.unwrap();
    let session = created["session_id"].as_str().unwrap().to_owned();
    client
        .call(
            "prompt.submit",
            json!({"session_id": session, "text": "build"}),
        )
        .await
        .unwrap();
    let mut answer = None;
    let mut notices = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_mins(1);
    let mut until = deadline;
    while let Ok(Some(event)) = tokio::time::timeout_at(until, inbox.events.recv()).await {
        match event.event.as_str() {
            "turn.finished" => {
                answer = Some(
                    event.data["final_response"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                );
                until = tokio::time::Instant::now() + linger;
            }
            "session.event" if event.data["type"] == "process.notice" => {
                notices.push(event.data["payload"].clone());
                if answer.is_some() {
                    break;
                }
            }
            _ => {}
        }
    }
    (answer.expect("the turn finished"), notices)
}

fn run(extra: Value, linger: Duration) -> (String, Vec<Value>) {
    let home = mock_home();
    let script = home.path().join("s.toml");
    std::fs::write(&script, SCRIPT).unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let (client, mut inbox) = daemon::connect(home.path(), "default").await;
        poll_twice(&client, &mut inbox, script.to_str().unwrap(), extra, linger).await
    })
}

/// Scenario: Second unchanged poll, in a daemon-hosted interactive session:
/// the second poll announces the notification and the exit later reaches
/// the session's clients as its own message.
#[test]
fn a_quiet_wait_exit_reaches_the_clients_of_an_interactive_session() {
    let (answer, notices) = run(json!({}), Duration::from_secs(20));
    assert_eq!(answer, "switched");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["kind"], "result");
    let text = notices[0]["text"].as_str().unwrap();
    assert!(text.contains("finished: sleep 2; echo built"), "{text}");
}

/// A one-shot session has nobody to tell later: polling stays plain and
/// no notice is sent.
#[test]
fn a_one_shot_session_keeps_polling_plain() {
    let (answer, notices) = run(json!({"source_tag": "oneshot"}), Duration::from_secs(4));
    assert_eq!(answer, "plain");
    assert!(notices.is_empty(), "{notices:?}");
}
