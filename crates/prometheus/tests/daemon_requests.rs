//! A skill's missing secret asked of the daemon session's clients (spec:
//! workspace-architecture, Server-to-client requests; skills, Skill
//! secrets and settings): a capable client answers `secret.request`, the
//! value lands in the profile's secrets store and never in the
//! transcript; with no capable client the request settles unanswered and
//! the secret is reported missing.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::Duration;

use pmt_rpc::client::{Client, Inbox};
use pmt_rpc::control::Purpose;
use serde_json::{Value, json};

use self::common::{daemon, mock_home, ok, run, session_messages};

const NOTION: &str = "---\nname: notion\ndescription: Notion pages.\nmetadata:\n  prometheus:\n    secrets:\n      - name: NOTION_API_KEY\n        prompt: Notion integration token\n---\nUse the API.\n";

const SCRIPT: &str = r#"[[rule]]
step = 0
[[rule.tool_calls]]
name = "skill_view"
arguments = { name = "notion" }

[[rule]]
step = 1
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "test -n \"$NOTION_API_KEY\" && echo KEY_SET || echo KEY_EMPTY" }

[[rule]]
step = 2
tool_result_regex = "KEY_SET"
text = "the secret reached the terminal"

[[rule]]
text = "the secret is missing"
"#;

const VALUE: &str = "ntn-daemon-value-4242";

/// A home with the `notion` skill; the mock script's path.
fn home_with_skill() -> (tempfile::TempDir, String) {
    let home = mock_home();
    let skill = home.path().join("profiles/default/skills/notion");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), NOTION).unwrap();
    let script = home.path().join("s.toml");
    std::fs::write(&script, SCRIPT).unwrap();
    let script = script.to_string_lossy().into_owned();
    (home, script)
}

async fn client(home: &Path, server_requests: bool) -> (Client, Inbox) {
    Client::connect(
        &daemon::record(home).endpoint,
        Purpose::Interactive,
        vec!["default".into()],
        &daemon::hello(server_requests),
    )
    .await
    .unwrap()
}

/// Runs "use notion" in a new session; answers a `secret.request` with
/// `VALUE`; returns the session, the requests seen and the final answer.
async fn turn(client: &Client, inbox: &mut Inbox, script: &str) -> (String, Vec<Value>, String) {
    let setup = json!({"setup": {"mock_script": script, "yolo": true}});
    let created = client.call("session.create", setup).await.unwrap();
    let session = created["session_id"].as_str().unwrap().to_owned();
    client
        .call(
            "prompt.submit",
            json!({"session_id": session, "text": "use notion"}),
        )
        .await
        .unwrap();
    let mut asked = Vec::new();
    loop {
        tokio::select! {
            Some(call) = inbox.requests.recv() => {
                assert_eq!(call.method, "secret.request");
                asked.push(call.params.clone());
                client.respond(&call.id, Ok(json!({"value": VALUE})));
            }
            Some(event) = inbox.events.recv() => {
                if event.event == "turn.finished" {
                    let answer = event.data["final_response"].as_str().unwrap_or_default();
                    return (session, asked, answer.to_owned());
                }
            }
        }
    }
}

fn run_turn(home: &Path, script: &str, server_requests: bool) -> (String, Vec<Value>, String) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let (client, mut inbox) = client(home, server_requests).await;
        tokio::time::timeout(Duration::from_mins(1), turn(&client, &mut inbox, script))
            .await
            .expect("the turn finished")
    })
}

/// Scenario: Secret value never reaches the transcript, in a daemon
/// session: the capable client is asked, the value is stored in the
/// profile's secrets store, reaches the skill's terminal, and is in no
/// stored message.
#[test]
fn a_capable_client_is_asked_for_a_missing_skill_secret() {
    let (home, script) = home_with_skill();
    let _daemon = daemon::start(home.path(), &[]);
    let (session, asked, answer) = run_turn(home.path(), &script, true);
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0]["name"], "NOTION_API_KEY");
    assert_eq!(asked[0]["reason"], "skill notion: Notion integration token");
    assert_eq!(answer, "the secret reached the terminal");
    let listed = ok(&run(home.path(), &["secrets", "list"]));
    assert!(listed.contains("NOTION_API_KEY"), "{listed}");
    let secrets =
        std::fs::read_to_string(home.path().join("profiles/default/secrets.toml")).unwrap();
    assert!(secrets.contains(VALUE), "stored in secrets.toml");
    let transcript = serde_json::to_string(&session_messages(home.path(), &session)).unwrap();
    assert!(transcript.contains("NOTION_API_KEY"), "{transcript}");
    assert!(!transcript.contains(VALUE), "{transcript}");
}

/// With no client that answers server requests the request settles at
/// once: the skill loads with the secret reported missing.
#[test]
fn without_a_capable_client_the_secret_stays_missing() {
    let (home, script) = home_with_skill();
    let _daemon = daemon::start(home.path(), &[]);
    let (_, asked, answer) = run_turn(home.path(), &script, false);
    assert!(asked.is_empty());
    assert_eq!(answer, "the secret is missing");
    let listed = ok(&run(home.path(), &["secrets", "list"]));
    assert!(!listed.contains("NOTION_API_KEY"), "{listed}");
}
