//! Background delegation in daemon sessions (spec: subagent-delegation,
//! Background and synchronous dispatch; Durable background completions):
//! an interactive client session dispatches its children in the
//! background, and a child's result survives a daemon restart, reaching
//! its session exactly once when the session is next loaded.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::Duration;

use pmt_rpc::client::{Client, Inbox};
use serde_json::{Value, json};

use self::common::{daemon, mock_home, run};

const SCRIPT: &str = r#"
[[rule]]
user_regex = "delegation_completed"
repeat = true
text = "got the completion"

[[rule]]
user = "please delegate"
[[rule.tool_calls]]
name = "delegate_task"
arguments = { tasks = [{ goal = "slow background work" }] }

[[rule]]
tool_result = "dispatched"
text = "dispatched it"

[[rule]]
user = "slow background work"
latency_ms = 60000
text = "the child finished"
"#;

/// Waits for the next `turn.finished`; returns its final response.
async fn finished(inbox: &mut Inbox) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_mins(1);
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, inbox.events.recv()).await {
        if event.event == "turn.finished" {
            return event.data["final_response"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
        }
    }
    panic!("no turn finished");
}

async fn submit(client: &Client, session: &str, text: &str) {
    client
        .call(
            "prompt.submit",
            json!({"session_id": session, "text": text}),
        )
        .await
        .unwrap();
}

fn delegation_rows(home: &Path) -> Vec<(String, String)> {
    let db = home.join("profiles/default/state.db");
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT id, delivery FROM delegations")
        .unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn child_status(home: &Path) -> String {
    let db = home.join("profiles/default/state.db");
    let conn = rusqlite::Connection::open(db).unwrap();
    let (status, result): (String, Option<String>) = conn
        .query_row("SELECT status, result FROM subagents", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    format!("{status} {}", result.unwrap_or_default())
}

/// Scenario: Background dispatch returns a handle; Restart delivers an
/// owned completion once.
#[test]
fn a_background_childs_result_survives_a_daemon_restart_and_arrives_once() {
    let home = mock_home();
    let script = home.path().join("s.toml");
    std::fs::write(&script, SCRIPT).unwrap();
    let setup = json!({"mock_script": script.to_str().unwrap()});
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let session = {
        let _daemon = daemon::start(home.path(), &[]);
        runtime.block_on(async {
            let (client, mut inbox) = daemon::connect(home.path(), "default").await;
            let created = client
                .call("session.create", json!({"setup": setup}))
                .await
                .unwrap();
            let session = created["session_id"].as_str().unwrap().to_owned();
            submit(&client, &session, "please delegate").await;
            assert_eq!(finished(&mut inbox).await, "dispatched it");
            session
        })
        // The daemon stops here while the child still runs.
    };
    let rows = delegation_rows(home.path());
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].1, "pending",
        "nothing was delivered before the restart"
    );
    let _daemon = daemon::start(home.path(), &[]);
    let status = child_status(home.path());
    assert!(status.starts_with("interrupted "), "{status}");
    let answers: Vec<String> = runtime.block_on(async {
        let (client, mut inbox) = daemon::connect(home.path(), "default").await;
        client
            .call(
                "session.resume",
                json!({"session_id": session, "setup": setup}),
            )
            .await
            .unwrap();
        let first = finished(&mut inbox).await;
        // Nothing more arrives: the completion was delivered once.
        let more = tokio::time::timeout(Duration::from_secs(3), inbox.events.recv()).await;
        let extra = matches!(more, Ok(Some(ref e)) if e.event == "turn.finished");
        vec![first, extra.to_string()]
    });
    assert_eq!(answers, ["got the completion", "false"]);
    assert_eq!(delegation_rows(home.path())[0].1, "delivered");
    let messages = common::session_messages(home.path(), &session);
    let completion: Vec<&Value> = messages
        .iter()
        .filter(|m| {
            m["role"] == "user"
                && m["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("delegation_completed"))
        })
        .collect();
    assert_eq!(completion.len(), 1, "{messages:?}");
    drop(run(home.path(), &["daemon", "stop"]));
}
