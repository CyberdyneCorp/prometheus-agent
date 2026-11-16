//! The TUI's M3 upgrades in a pseudo-terminal against the real daemon
//! and the mock provider (spec: tui, Split panes for subagents and tool
//! output; Inline diff review for file edits; Session timeline scrubber;
//! Shared live sessions): watching a delegated child in the subagents
//! pane, accepting some hunks of an edit, rewinding to an earlier turn
//! (and restoring that turn's files), observing a session read-only, and
//! a desktop window joining the session the TUI drives.

#![cfg(all(unix, feature = "gateway", feature = "tui"))]

mod common;

use std::path::Path;
use std::time::Duration;

use common::tui::{enter, finish, opened, see, see_repainted, tui, tui_at};
use common::{daemon, mock_home, ok, run, temp_home};
use expectrl::Expect as _;
use serde_json::{Value, json};

const UP: &str = "\u{1b}[A";
const DOWN: &str = "\u{1b}[B";
const F2: &str = "\u{1b}OQ";
const CTRL_T: &str = "\u{14}";

fn script(dir: &Path, text: &str) -> String {
    let path = dir.join("script.toml");
    std::fs::write(&path, text).unwrap();
    path.to_string_lossy().into_owned()
}

/// Every session of the home, newest first: `(id, user messages)`.
fn sessions(home: &Path) -> Vec<(String, usize)> {
    let listed = ok(&run(home, &["--json", "sessions", "list", "--all"]));
    let listed: Value = serde_json::from_str(&listed).unwrap();
    listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            let id = s["id"].as_str().unwrap().to_owned();
            let users = common::session_messages(home, &id)
                .iter()
                .filter(|m| m["role"] == "user" && m["active"] == true)
                .count();
            (id, users)
        })
        .collect()
}

const DELEGATION: &str = r#"
[[rule]]
user_regex = "delegation_completed"
repeat = true
text = "noted the results"

[[rule]]
user = "split the work"
[[rule.tool_calls]]
name = "delegate_task"
arguments = { tasks = [{ goal = "first child job" }, { goal = "second child job" }] }

[[rule]]
tool_result = "dispatched"
text = "dispatched both"

[[rule]]
user = "first child job"
text = "first child finished"

[[rule]]
user = "second child job"
latency_ms = 1500
[[rule.tool_calls]]
name = "read_file"
arguments = { path = "notes.txt" }

[[rule]]
tool_result = "remember the milk"
latency_ms = 1500
text = "second child read the notes"
"#;

/// Scenario: Watching a delegated child.
#[test]
fn f2_lists_both_children_and_streams_the_selected_ones_transcript_to_its_end() {
    let home = mock_home();
    let work = temp_home();
    std::fs::write(work.path().join("notes.txt"), "remember the milk\n").unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let script = script(work.path(), DELEGATION);
    let mut p = tui_at(
        home.path(),
        work.path(),
        &["chat", "--mock-script", &script],
        (100, 40),
    );
    opened(&mut p);
    enter(&mut p, "split the work");
    see(&mut p, "dispatched both");
    p.send(F2).unwrap();
    see_repainted(&mut p, "subagents (2)");
    see_repainted(&mut p, "2. second child job");
    p.send(DOWN).unwrap();
    // The second child's tool call, its answer and how it ended stream in.
    see_repainted(&mut p, "· read_file notes.txt");
    see_repainted(&mut p, "second child read the notes");
    see_repainted(&mut p, "■ completed");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
}

const EDIT: &str = r#"
[[rule]]
user = "tidy the notes"
[[rule.tool_calls]]
name = "read_file"
arguments = { path = "AGENTS.md" }

[[rule]]
user = "tidy the notes"
[[rule.tool_calls]]
name = "write_file"
arguments = { path = "AGENTS.md", content = "a\nB\nc\nd\ne\nf\ng\nh\ni\nJ\nk\nl\nm\nn\no\np\nq\nR\ns\nt\n" }

[[rule]]
tool_result = "hunk 2 was rejected by the user"
text = "kept two of three"
"#;

/// Scenario: Partial acceptance.
#[test]
fn accepting_hunks_1_and_3_writes_only_those_and_the_model_hears_hunk_2_was_rejected() {
    let home = mock_home();
    let work = temp_home();
    let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nt\n";
    std::fs::write(work.path().join("AGENTS.md"), old).unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let script = script(work.path(), EDIT);
    let mut p = tui_at(
        home.path(),
        work.path(),
        &["chat", "--mock-script", &script],
        (100, 50),
    );
    opened(&mut p);
    enter(&mut p, "tidy the notes");
    see(&mut p, "[1] @@ -1,4 +1,4 @@");
    see(&mut p, "[3] @@");
    see(&mut p, "hunks 1,3: accept those");
    enter(&mut p, "hunks 1,3");
    see(&mut p, "kept two of three");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
    let session = common::newest_session(home.path());
    let tool = common::session_messages(home.path(), &session)
        .into_iter()
        .rfind(|m| m["role"] == "tool")
        .unwrap();
    let text = tool["content"].as_str().unwrap();
    let written = std::fs::read_to_string(work.path().join("AGENTS.md")).unwrap();
    assert_eq!(
        written,
        old.replace("b\n", "B\n").replace("r\n", "R\n"),
        "{text}"
    );
    assert!(
        text.contains("applied hunks 1, 3; hunk 2 was rejected by the user"),
        "{text}"
    );
}

/// Scenario: Rewind keeps the original.
#[test]
fn rewinding_a_12_turn_session_to_turn_7_shows_a_7_turn_branch_and_keeps_the_original() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    for n in 1..=12 {
        enter(&mut p, &format!("step {n}"));
        see(&mut p, &format!("mock: step {n}"));
    }
    let original = common::newest_session(home.path());
    p.send(CTRL_T).unwrap();
    see(&mut p, "turn 12 (read-only preview)");
    for _ in 0..5 {
        p.send(UP).unwrap();
    }
    // Each move previews the highlighted turn read-only (the screen
    // snapshots show it); the live session is untouched, as the store
    // shows below.
    std::thread::sleep(Duration::from_millis(500));
    p.send("\r").unwrap();
    let rewound = see(&mut p, "rewound to turn 7");
    assert!(!rewound.contains("ERROR"), "{rewound}");
    see_repainted(&mut p, "2 live");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
    let all = sessions(home.path());
    let users = |id: &str| all.iter().find(|(s, _)| s == id).unwrap().1;
    assert_eq!(users(&original), 12, "the original is unchanged");
    // Besides hidden auxiliary sessions, the branch: turns 1 to 7.
    let branches: Vec<_> = all
        .iter()
        .filter(|(s, n)| *s != original && *n > 1)
        .collect();
    assert_eq!(branches.len(), 1, "{all:?}");
    assert_eq!(branches[0].1, 7, "the branch ends at turn 7");
}

const WRITES: &str = r#"
[[rule]]
user = "write v1"
[[rule.tool_calls]]
name = "write_file"
arguments = { path = "a.txt", content = "v1\n" }

[[rule]]
user = "write v2"
[[rule.tool_calls]]
name = "write_file"
arguments = { path = "a.txt", content = "v2\n" }

[[rule]]
user = "write v3"
[[rule.tool_calls]]
name = "write_file"
arguments = { path = "a.txt", content = "v3\n" }

[[rule]]
tool_result = "a.txt"
repeat = true
text = "written"
"#;

#[test]
fn a_rewind_offers_the_turns_checkpoint_and_yes_restores_its_files() {
    let home = mock_home();
    let work = temp_home();
    let _daemon = daemon::start(home.path(), &[]);
    let script = script(work.path(), WRITES);
    let mut p = tui_at(
        home.path(),
        work.path(),
        &["chat", "--mock-script", &script],
        (100, 30),
    );
    opened(&mut p);
    let file = work.path().join("a.txt");
    for n in 1..=3 {
        enter(&mut p, &format!("write v{n}"));
        // "written" from an earlier turn may already be on screen: repaint
        // until this turn's write is there.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            see_repainted(&mut p, "written");
            if std::fs::read_to_string(&file).ok() == Some(format!("v{n}\n")) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "turn {n} never wrote");
        }
    }
    p.send(CTRL_T).unwrap();
    see(&mut p, "timeline: ↑↓ preview");
    p.send(UP).unwrap();
    p.send(UP).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    p.send("\r").unwrap();
    see(&mut p, "restore the workspace to turn 1's files?");
    enter(&mut p, "y");
    see(&mut p, "restored:");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
    assert_eq!(
        std::fs::read_to_string(work.path().join("a.txt")).unwrap(),
        "v1\n"
    );
}

/// Scenario: Observer cannot submit.
#[test]
fn a_tui_observing_a_session_shows_it_read_only_and_cannot_submit() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (owner, _inbox, session) = runtime.block_on(async {
        let (client, inbox) = daemon::connect(home.path(), "default").await;
        let created = client.call("session.create", json!({})).await.unwrap();
        let session = created["session_id"].as_str().unwrap().to_owned();
        (client, inbox, session)
    });
    let mut p = tui(home.path(), &["tui", "--observe", &session], &[]);
    opened(&mut p);
    see_repainted(&mut p, "observing");
    see_repainted(&mut p, "with cli-");
    enter(&mut p, "hello");
    see(&mut p, "observers cannot submit input");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
    drop(owner);
    let messages = common::session_messages(home.path(), &session);
    assert!(
        messages.iter().all(|m| m["role"] != "user"),
        "the observer's input never reached the session: {messages:?}"
    );
}

/// Scenario: Desktop joins a TUI session.
#[test]
fn a_desktop_joining_the_tuis_session_is_shown_and_its_input_labelled() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    enter(&mut p, "first from the tui");
    see(&mut p, "mock: first from the tui");
    let session = common::newest_session(home.path());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (desktop, mut inbox, me) = runtime.block_on(async {
        let (client, inbox) = daemon::desktop(home.path(), "default", &[]).await;
        let attached = client
            .call("session.attach", json!({"session_id": session}))
            .await
            .unwrap();
        let me = attached["client_id"].as_str().unwrap().to_owned();
        (client, inbox, me)
    });
    see(&mut p, &format!("{me} joined as driver"));
    see_repainted(&mut p, &format!("with {me}"));
    let presence = runtime.block_on(async {
        loop {
            let event = tokio::time::timeout(Duration::from_secs(10), inbox.events.recv())
                .await
                .unwrap()
                .unwrap();
            if event.event == "session.presence" {
                return event.data;
            }
        }
    });
    let kinds: Vec<&str> = presence["clients"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["client_kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["tui", "desktop"]);
    runtime.block_on(async {
        desktop
            .call(
                "prompt.submit",
                json!({"session_id": session, "text": "hello from the desk"}),
            )
            .await
            .unwrap();
    });
    see(&mut p, &format!("[{me}] hello from the desk"));
    see(&mut p, "mock: hello from the desk");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
}
