//! Task 81.3, computer use: a desktop session of the daemon drives a
//! stand-in desktop (the `cua-driver` fixture) through the
//! `computer_use` tool; the capture runs freely, and each input action
//! waits for the person's answer to its approval request: the approved
//! click reaches the desktop, the denied typing never does (spec:
//! computer-use, Computer use tool; Input actions need approval).

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::Duration;

use pmt_rpc::client::{Client, Inbox};
use serde_json::{Value, json};

use crate::common::{append_config, daemon, mock_home};

const DRIVE: &str = r#"
[[rule]]
user = "tidy my desktop"
[[rule.tool_calls]]
name = "computer_use"
arguments = { action = "capture", mode = "som" }

[[rule]]
step = 1
[[rule.tool_calls]]
name = "computer_use"
arguments = { action = "click", element = 2 }

[[rule]]
step = 2
[[rule.tool_calls]]
name = "computer_use"
arguments = { action = "type", text = "hello" }

[[rule]]
repeat = true
text = "desktop tidied"
"#;

/// The stand-in driver's log: one JSON line per call it received.
fn driver_calls(bin: &Path, name: &str) -> Vec<Value> {
    std::fs::read_to_string(bin.join("log.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|e| e["call"] == name)
        .collect()
}

/// Answers the next approval request with `decision`; the request's
/// params.
async fn answer(client: &Client, inbox: &mut Inbox, decision: &str) -> Value {
    let request = tokio::time::timeout(Duration::from_mins(1), inbox.requests.recv())
        .await
        .expect("an approval request within a minute")
        .expect("the connection is open");
    assert_eq!(request.method, "approval.request", "{request:?}");
    client.respond(&request.id, Ok(json!({"decision": decision})));
    request.params
}

/// Waits for the session's `turn.finished`.
async fn finished(inbox: &mut Inbox, session: &str) {
    loop {
        let event = tokio::time::timeout(Duration::from_mins(1), inbox.events.recv())
            .await
            .expect("the turn finishes within a minute")
            .expect("the connection is open");
        if event.session_id.as_deref() == Some(session) && event.event == "turn.finished" {
            return;
        }
    }
}

#[test]
fn computer_use_drives_a_mock_desktop_under_approval() {
    let home = mock_home();
    let bin = home.path().join("driver-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let driver = bin.join("cua-driver");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/computer-use/cua-driver"),
        &driver,
    )
    .unwrap();
    std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(bin.join("fake.json"), "{}").unwrap();
    std::fs::write(home.path().join("drive.toml"), DRIVE).unwrap();
    append_config(
        home.path(),
        &format!(
            "\n[providers.mock]\nscript = {:?}\n[tools]\nenabled_toolsets = [\"computer_use\"]\n\
             [computer_use]\ndriver_path = {:?}\n",
            home.path().join("drive.toml").display().to_string(),
            driver.display().to_string()
        ),
    );
    let state = home.path().join("fake-state");
    let _daemon = daemon::start_env(
        home.path(),
        &[
            ("DISPLAY", ":42"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/dev/null"),
            ("XDG_RUNTIME_DIR", state.to_str().unwrap()),
        ],
    );

    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let (client, mut inbox) = daemon::desktop(home.path(), "default", &[]).await;
        let created = client
            .call("session.create", json!({"profile": "default"}))
            .await
            .unwrap();
        let session = created["session_id"].as_str().unwrap().to_owned();
        client
            .call(
                "session.attach",
                json!({"session_id": session, "role": "driver"}),
            )
            .await
            .unwrap();
        client
            .call(
                "prompt.submit",
                json!({"session_id": session, "text": "tidy my desktop", "files": []}),
            )
            .await
            .unwrap();
        let click = answer(&client, &mut inbox, "once").await;
        assert!(click.to_string().contains("click"), "{click}");
        let typing = answer(&client, &mut inbox, "deny").await;
        assert!(typing.to_string().contains("type"), "{typing}");
        finished(&mut inbox, &session).await;
    });

    assert!(
        !driver_calls(&bin, "get_window_state").is_empty(),
        "the capture ran without asking"
    );
    let clicks = driver_calls(&bin, "click");
    assert_eq!(clicks.len(), 1, "the approved click reached the desktop");
    assert_eq!(clicks[0]["arguments"]["element_index"], 2);
    assert!(
        driver_calls(&bin, "type_text").is_empty(),
        "the denied typing never reached the desktop"
    );
}
