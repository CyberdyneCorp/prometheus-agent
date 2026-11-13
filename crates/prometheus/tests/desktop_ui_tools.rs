//! The daemon side of the desktop UI toolset (spec: desktop-app, Desktop
//! UI toolset contract; Agent reactions; Preview pane tool; Pane focus,
//! tours and tips; First-run onboarding chat; projects, Project tool for
//! GUI sessions): real daemon sessions driven by the mock model, with a
//! protocol client standing in for the desktop window.

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{daemon, mock_home};
use pmt_rpc::client::{Client, Inbox};
use serde_json::{Value, json};

/// The `ui.*` requests a stand-in window received, in order.
type Seen = Arc<Mutex<Vec<(String, Value)>>>;

/// Answers the window's `ui.*` requests with `answer` (`None`: never) and
/// approvals with `once`; every request is recorded.
fn window(
    inbox: Inbox,
    answer: fn(&str, &Value) -> Option<Value>,
) -> (
    Seen,
    tokio::sync::mpsc::UnboundedReceiver<pmt_rpc::contracts::Envelope>,
    Arc<Mutex<Option<Client>>>,
) {
    let seen: Seen = Arc::default();
    let client: Arc<Mutex<Option<Client>>> = Arc::default();
    let Inbox {
        events,
        mut requests,
    } = inbox;
    let (record, slot) = (Arc::clone(&seen), Arc::clone(&client));
    pmt_core::spawn::spawn(async move {
        while let Some(call) = requests.recv().await {
            record
                .lock()
                .unwrap()
                .push((call.method.clone(), call.params.clone()));
            let reply = if call.method == "approval.request" {
                Some(json!({"decision": "once"}))
            } else {
                answer(&call.method, &call.params["arguments"])
            };
            if let (Some(reply), Some(client)) = (reply, slot.lock().unwrap().as_ref()) {
                client.respond(&call.id, Ok(reply));
            }
        }
    });
    (seen, events, client)
}

async fn create(client: &Client, cwd: Option<&Path>, script: &Path) -> String {
    let mut params = json!({"setup": {"mock_script": script.to_string_lossy()}});
    if let Some(cwd) = cwd {
        params["cwd"] = json!(cwd.to_string_lossy());
    }
    let created = client.call("session.create", params).await.unwrap();
    created["session_id"].as_str().unwrap().to_owned()
}

/// Submits `text` and waits for its `turn.finished`.
async fn turn(
    client: &Client,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<pmt_rpc::contracts::Envelope>,
    session: &str,
    text: &str,
) {
    client
        .call(
            "prompt.submit",
            json!({"session_id": session, "text": text}),
        )
        .await
        .unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_mins(1), events.recv())
            .await
            .expect("the turn finishes within 60 s")
            .expect("the connection is open");
        if event.event == "turn.finished" && event.session_id.as_deref() == Some(session) {
            return;
        }
    }
}

async fn transcript(client: &Client, session: &str) -> Value {
    client
        .call("session.transcript", json!({"session_id": session}))
        .await
        .unwrap()
}

fn tools_of(home: &Path, session: &str) -> String {
    common::ok(&common::run(home, &["tools", "list", "--session", session]))
}

fn exposed(listing: &str, tool: &str) -> bool {
    listing
        .lines()
        .any(|l| l.split_whitespace().next() == Some(tool) && l.contains(" exposed"))
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

const WINDOW_SCRIPT: &str = r#"
[[rule]]
user = "read the terminal"
[[rule.tool_calls]]
name = "read_terminal"
arguments = {}

[[rule]]
tool_result = "read_terminal needs an open Prometheus Desktop window"
text = "carried on without a window"

[[rule]]
user = "open the metadata page"
[[rule.tool_calls]]
name = "desktop_preview"
arguments = { action = "open", url = "http://169.254.169.254/latest/meta-data" }

[[rule]]
user = "focus the inbox"
[[rule.tool_calls]]
name = "focus_pane"
arguments = { pane = "inbox" }

[[rule]]
user = "tour both ways"
[[rule.tool_calls]]
name = "gui_tour"
arguments = { action = "start", preset = "quick", steps = [{ selector = "form.composer" }] }

[[rule]]
user = "peek again"
[[rule.tool_calls]]
name = "read_terminal"
arguments = { count = 2 }

[[rule]]
repeat = true
text = "ok"
"#;

fn terminal_screen(method: &str, _args: &Value) -> Option<Value> {
    (method == "ui.read_terminal").then(|| {
        json!({"success": true, "total_lines": 2, "start": 0, "end": 2, "viewport_rows": 24, "cursor_row": 1, "text": "$ npm run dev\nready on :5173"})
    })
}

/// Scenario: No window attached. Scenario: Blocked private address.
/// Scenario: Unknown pane. Scenario: Preset and steps together.
#[test]
fn window_tools_need_a_window_and_refused_calls_never_reach_it() {
    let home = mock_home();
    let script = home.path().join("window.toml");
    std::fs::write(&script, WINDOW_SCRIPT).unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    runtime().block_on(async {
        // The session's window was closed: its desktop connection does not
        // render the UI tools.
        let (closed, mut closed_inbox) = daemon::desktop(home.path(), "default", &[]).await;
        let session = create(&closed, None, &script).await;
        turn(
            &closed,
            &mut closed_inbox.events,
            &session,
            "read the terminal",
        )
        .await;
        let text = transcript(&closed, &session).await.to_string();
        assert!(
            text.contains(r#"\"error\":\"read_terminal needs an open Prometheus Desktop window\""#)
                && text.contains(r#"\"success\":false"#),
            "{text}"
        );
        assert!(
            text.contains("carried on without a window"),
            "the turn continued: {text}"
        );

        let (open, inbox) = daemon::desktop(home.path(), "default", &["desktop_ui"]).await;
        let (seen, mut events, slot) = window(inbox, terminal_screen);
        open.call("session.attach", json!({"session_id": session}))
            .await
            .unwrap();
        *slot.lock().unwrap() = Some(open);
        let client = slot.lock().unwrap().clone().unwrap();
        for prompt in [
            "open the metadata page",
            "focus the inbox",
            "tour both ways",
        ] {
            turn(&client, &mut events, &session, prompt).await;
        }
        let text = transcript(&client, &session).await.to_string();
        assert!(text.contains("cloud metadata endpoint"), "{text}");
        assert!(
            text.contains(
                "unknown pane 'inbox'; use one of chat, files, terminal, review, sessions"
            ),
            "{text}"
        );
        assert!(text.contains("pass steps or preset, not both"), "{text}");
        assert!(
            seen.lock().unwrap().is_empty(),
            "refused calls never reach the window: {seen:?}"
        );

        turn(&client, &mut events, &session, "peek again").await;
        let text = transcript(&client, &session).await.to_string();
        assert!(text.contains("ready on :5173"), "{text}");
        let sent = seen.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].0, "ui.read_terminal");
        assert_eq!(sent[0].1["arguments"], json!({"count": 2}));
        assert_eq!(sent[0].1["deadline_ms"], 30_000);
    });
}

const REACT_SCRIPT: &str = r#"
[[rule]]
user = "react to my first message"
[[rule.tool_calls]]
name = "react_to_message"
arguments = { emoji = "😂", messages_back = 1 }

[[rule]]
user = "react to a foreign message"
[[rule.tool_calls]]
name = "react_to_message"
arguments = { emoji = "👍", message_id = "FOREIGN" }

[[rule]]
repeat = true
text = "ok"
"#;

/// Scenario: Reacting to an earlier message. Scenario: Foreign message id.
#[test]
fn the_agent_reacts_to_its_own_conversation_only() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    runtime().block_on(async {
        let (client, inbox) = daemon::desktop(home.path(), "default", &["desktop_ui"]).await;
        let Inbox { mut events, .. } = inbox;
        // Reactions are off by default; the user turns them on first.
        client
            .call(
                "ui.prefs.patch",
                json!({"set": {"message_reactions": true}}),
            )
            .await
            .unwrap();
        let plain = home.path().join("plain.toml");
        std::fs::write(&plain, "[[rule]]\nrepeat = true\ntext = \"ok\"\n").unwrap();
        let other = create(&client, None, &plain).await;
        turn(&client, &mut events, &other, "elsewhere").await;
        let foreign = transcript(&client, &other).await["messages"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let script = home.path().join("react.toml");
        std::fs::write(&script, REACT_SCRIPT.replace("FOREIGN", &foreign)).unwrap();
        let session = create(&client, None, &script).await;
        turn(&client, &mut events, &session, "hello there").await;
        turn(&client, &mut events, &session, "react to my first message").await;
        let messages = transcript(&client, &session).await["messages"].clone();
        let users: Vec<&Value> = messages
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "user")
            .collect();
        assert_eq!(users[0]["text"], "hello there");
        assert_eq!(
            users[0]["reactions"],
            json!([{"by": "agent", "emoji": "😂"}]),
            "{messages}"
        );
        assert!(users[1].get("reactions").is_none(), "{messages}");

        turn(&client, &mut events, &session, "react to a foreign message").await;
        let text = transcript(&client, &session).await.to_string();
        assert!(
            text.contains(&format!(
                "message {foreign} is not part of this conversation"
            )),
            "{text}"
        );
    });
}

/// Scenario: Turning tips off mid-session. Scenario: Regular chats lack
/// the tool.
#[test]
fn opt_outs_apply_from_the_next_session_and_only_onboarding_gets_setup_beat() {
    let home = mock_home();
    let script = home.path().join("plain.toml");
    std::fs::write(
        &script,
        "[[rule]]\nuser = \"forge\"\n[[rule.tool_calls]]\nname = \"setup_beat\"\narguments = { beat = \"name\" }\n\n[[rule]]\nrepeat = true\ntext = \"ok\"\n",
    )
    .unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    runtime().block_on(async {
        let (client, inbox) = daemon::desktop(home.path(), "default", &["desktop_ui"]).await;
        let (seen, mut events, slot) = window(inbox, |_, _| Some(json!({"success": true})));
        *slot.lock().unwrap() = Some(client);
        let client = slot.lock().unwrap().clone().unwrap();
        let first = create(&client, None, &script).await;
        turn(&client, &mut events, &first, "hello").await;
        let before = tools_of(home.path(), &first);
        assert!(exposed(&before, "show_tip") && exposed(&before, "gui_tour"), "{before}");
        assert!(!exposed(&before, "react_to_message"), "reactions are off by default: {before}");

        client
            .call("ui.prefs.patch", json!({"set": {"tips_enabled": false}}))
            .await
            .unwrap();
        turn(&client, &mut events, &first, "still here").await;
        assert!(exposed(&tools_of(home.path(), &first), "show_tip"), "the running session keeps it");
        let next = create(&client, None, &script).await;
        turn(&client, &mut events, &next, "hello").await;
        let after = tools_of(home.path(), &next);
        assert!(!exposed(&after, "show_tip") && exposed(&after, "gui_tour"), "{after}");
        assert!(!exposed(&after, "setup_beat"), "{after}");

        // A forged call from a regular chat never reaches the window.
        turn(&client, &mut events, &next, "forge").await;
        assert!(
            seen.lock().unwrap().iter().all(|(m, _)| m != "ui.setup_beat"),
            "{seen:?}"
        );

        let onboarding = client
            .call("onboarding.ensure", json!({"profile": "default"}))
            .await
            .unwrap();
        let id = onboarding["session_id"].as_str().unwrap().to_owned();
        assert_eq!(onboarding["created"], true);
        let again = client
            .call("onboarding.ensure", json!({"profile": "default"}))
            .await
            .unwrap();
        assert_eq!(again["session_id"], id.as_str(), "one onboarding chat");
        client.call("session.attach", json!({"session_id": id})).await.unwrap();
        turn(&client, &mut events, &id, "hello").await;
        assert!(exposed(&tools_of(home.path(), &id), "setup_beat"));
        let refused = client
            .call(
                "session.create",
                json!({"source_tag": "onboarding", "setup": {"mock_script": script.to_string_lossy()}}),
            )
            .await;
        assert!(refused.is_err(), "the tag is the daemon's: {refused:?}");
    });
}

const ONBOARDING_SCRIPT: &str = r#"
[[rule]]
user = "Hi! Please help me set up Prometheus."
[[rule.tool_calls]]
name = "setup_beat"
arguments = { beat = "name", question = "What should I call you?" }

[[rule]]
tool_result = "Leo"
[[rule.tool_calls]]
name = "setup_beat"
arguments = { beat = "theme" }

[[rule]]
tool_result = "timed out"
text = "keeping the default theme"

[[rule]]
repeat = true
text = "ok"
"#;

fn answers_the_name_only(method: &str, args: &Value) -> Option<Value> {
    (method == "ui.setup_beat" && args["beat"] == "name").then(|| json!({"answer": "Leo"}))
}

/// Scenario: Card deadline.
#[test]
fn an_unanswered_setup_card_times_out_and_keeps_the_defaults() {
    let home = mock_home();
    let script = home.path().join("profiles/default/onboarding.toml");
    std::fs::write(&script, ONBOARDING_SCRIPT).unwrap();
    common::append_config(
        home.path(),
        "\n[providers.mock]\nscript = \"onboarding.toml\"\n",
    );
    // 300 s in a release build; a debug daemon takes the test's deadline.
    let _daemon = daemon::start_env(home.path(), &[("PMT_TEST_UI_USER_WAIT_SECS", "2")]);
    runtime().block_on(async {
        let (client, inbox) = daemon::desktop(home.path(), "default", &["desktop_ui"]).await;
        let (seen, mut events, slot) = window(inbox, answers_the_name_only);
        *slot.lock().unwrap() = Some(client);
        let client = slot.lock().unwrap().clone().unwrap();
        let id = client
            .call("onboarding.ensure", json!({"profile": "default"}))
            .await
            .unwrap()["session_id"]
            .as_str()
            .unwrap()
            .to_owned();
        client
            .call("session.attach", json!({"session_id": id}))
            .await
            .unwrap();
        turn(
            &client,
            &mut events,
            &id,
            "Hi! Please help me set up Prometheus.",
        )
        .await;
        let text = transcript(&client, &id).await.to_string();
        assert!(
            text.contains("the theme card timed out after 2 s; the defaults stay in place"),
            "{text}"
        );
        assert!(text.contains("keeping the default theme"), "{text}");
        let beats: Vec<Value> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, p)| p["arguments"]["beat"].clone())
            .collect();
        assert_eq!(beats, [json!("name"), json!("theme")]);
        let record: Value = serde_json::from_slice(
            &std::fs::read(
                home.path()
                    .join(format!("profiles/default/onboarding/{id}.json")),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(record["beats"]["name"], "Leo");
        assert_eq!(record["beats"]["theme"]["timed_out"], true);
    });
}

const PROJECT_SCRIPT: &str = r#"
[[rule]]
user = "work on web"
[[rule.tool_calls]]
name = "project"
arguments = { action = "switch", project = "web" }

[[rule]]
tool_result = "from your next tool call"
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "pwd" }

[[rule]]
tool_result_regex = "/code/web"
text = "now in web"

[[rule]]
repeat = true
text = "ok"
"#;

fn prompt_hash(home: &Path, session: &str) -> Option<String> {
    let db = rusqlite::Connection::open_with_flags(
        home.join("profiles/default/state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    db.query_row(
        "SELECT system_prompt_hash FROM sessions WHERE id = ?1",
        [session],
        |row| row.get(0),
    )
    .unwrap()
}

/// Scenario: Switch keeps prompt cache (projects). Scenario: Switching
/// project (desktop-app, the daemon side: the session's project and the
/// `session.project` event).
#[test]
fn switching_project_moves_the_next_command_and_keeps_the_prompt() {
    let home = mock_home();
    let code = home.path().canonicalize().unwrap().join("code");
    for name in ["api", "web"] {
        std::fs::create_dir_all(code.join(name)).unwrap();
        common::ok(&common::run(
            home.path(),
            &[
                "projects",
                "create",
                name,
                code.join(name).to_str().unwrap(),
            ],
        ));
    }
    let script = home.path().join("project.toml");
    std::fs::write(&script, PROJECT_SCRIPT).unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    runtime().block_on(async {
        let (client, inbox) = daemon::desktop(home.path(), "default", &["desktop_ui"]).await;
        let Inbox { mut events, .. } = inbox;
        let session = create(&client, Some(&code.join("api")), &script).await;
        turn(&client, &mut events, &session, "hello").await;
        let before = prompt_hash(home.path(), &session).expect("the first turn set it");
        client
            .call(
                "prompt.submit",
                json!({"session_id": session, "text": "work on web"}),
            )
            .await
            .unwrap();
        let mut moved = None;
        loop {
            let event = tokio::time::timeout(Duration::from_mins(1), events.recv())
                .await
                .unwrap()
                .unwrap();
            if event.event == "session.project" {
                moved = Some(event.data.clone());
            }
            if event.event == "turn.finished" {
                break;
            }
        }
        let moved = moved.expect("the surface is told");
        assert_eq!(moved["slug"], "web");
        let text = transcript(&client, &session).await.to_string();
        assert!(text.contains("now in web"), "{text}");
        assert_eq!(
            prompt_hash(home.path(), &session).as_deref(),
            Some(before.as_str())
        );
        let listed = client
            .call("session.list", json!({"profile": "default"}))
            .await
            .unwrap();
        let row = listed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["session_id"] == session.as_str())
            .unwrap()
            .clone();
        assert_eq!(row["project_id"], moved["project_id"]);
    });
}
