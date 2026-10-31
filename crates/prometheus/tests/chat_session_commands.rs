//! Local transcript and session commands, retry, the queue and the
//! session browser (spec: cli, Local transcript and session commands;
//! Retry semantics; Queue management commands; session-storage,
//! Interactive session browser).

#![cfg(unix)]

mod common;

use std::time::Duration;

use common::pty::{exit, prompt, spawn};
use common::{command, mock_home, newest_session, ok, run, session_messages};
use expectrl::Expect;

/// Model calls recorded so far in the profile.
fn calls(home: &std::path::Path) -> u64 {
    let out = ok(&run(home, &["--json", "usage", "--by", "day"]));
    let usage: serde_json::Value = serde_json::from_str(&out).unwrap();
    usage["total"]["calls"].as_u64().unwrap_or(0)
}

/// Scenario: Status without model call; `/history`, `/version`,
/// `/sessions`.
#[test]
fn local_commands_answer_without_a_model_call() {
    let home = mock_home();
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("hello there").unwrap();
    p.expect("mock: hello there").unwrap();
    prompt(&mut p);
    let before = calls(home.path());
    p.send_line("/status").unwrap();
    p.expect("provider      mock").unwrap();
    p.expect("state         idle").unwrap();
    p.expect("turns         1").unwrap();
    p.expect("last prompt   hello there").unwrap();
    p.expect("last reply    mock: hello there").unwrap();
    prompt(&mut p);
    p.send_line("/history 1").unwrap();
    p.expect("assistant: mock: hello there").unwrap();
    prompt(&mut p);
    p.send_line("/history").unwrap();
    p.expect("you: hello there").unwrap();
    prompt(&mut p);
    p.send_line("/version").unwrap();
    p.expect("prometheus 0.").unwrap();
    prompt(&mut p);
    p.send_line("/sessions").unwrap();
    p.expect("(current)").unwrap();
    prompt(&mut p);
    p.send_line("/sessions search hello").unwrap();
    p.expect("sessions matching the search").unwrap();
    p.expect("(current)").unwrap();
    prompt(&mut p);
    assert_eq!(calls(home.path()), before, "no provider request");
    exit(p);
}

/// Scenario: Retry replaces the answer.
#[test]
fn retry_replaces_the_answer() {
    let home = mock_home();
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("/retry").unwrap();
    p.expect("nothing to retry").unwrap();
    prompt(&mut p);
    p.send_line("question").unwrap();
    p.expect("mock: question").unwrap();
    prompt(&mut p);
    p.send_line("/retry").unwrap();
    p.expect("mock: question").unwrap();
    prompt(&mut p);
    exit(p);
    let messages = session_messages(home.path(), &newest_session(home.path()));
    let answers: Vec<_> = messages
        .iter()
        .filter(|m| m["role"] == "assistant")
        .collect();
    assert_eq!(answers.len(), 2, "{messages:?}");
    assert_eq!(answers[0]["active"], false);
    assert_eq!(answers[1]["active"], true);
    let active_users = messages
        .iter()
        .filter(|m| m["role"] == "user" && m["active"] == true)
        .count();
    assert_eq!(active_users, 1);
}

/// Scenario: Reorder queue; queued prompts run in order after the turn.
#[test]
fn the_queue_is_listed_edited_reordered_and_run_in_order() {
    let home = mock_home();
    let script = home.path().join("slow.toml");
    std::fs::write(
        &script,
        "[[rule]]\nuser = \"slow\"\nlatency_ms = 2500\ntext = \"done slowly\"\n\n[[rule]]\nuser = \"x-c\"\ntext = \"got c\"\n\n[[rule]]\nuser = \"x-a\"\ntext = \"got a\"\n\n[[rule]]\nuser = \"x-b\"\ntext = \"got b\"\n",
    )
    .unwrap();
    let mut p = spawn(home.path(), &["--mock-script", script.to_str().unwrap()]);
    prompt(&mut p);
    p.send_line("slow").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    for item in ["/queue x-a", "/q x-b", "/queue x-c", "/queue zzz"] {
        p.send_line(item).unwrap();
        p.expect("queued (").unwrap();
    }
    p.send_line("/queue move 3 1").unwrap();
    p.expect("1. x-c").unwrap();
    p.send_line("/queue rm 9").unwrap();
    p.expect("no queued item 9").unwrap();
    p.send_line("/queue rm 4").unwrap();
    p.expect("3. x-b").unwrap();
    p.send_line("/queue list").unwrap();
    p.expect("1. x-c").unwrap();
    p.expect("2. x-a").unwrap();
    p.expect("3. x-b").unwrap();
    p.expect("done slowly").unwrap();
    p.expect("got c").unwrap();
    p.expect("got a").unwrap();
    p.expect("got b").unwrap();
    prompt(&mut p);
    p.send_line("/queue").unwrap();
    p.expect("the queue is empty").unwrap();
    prompt(&mut p);
    exit(p);
}

/// Scenario: Status tags; Enter resumes the chosen session.
#[test]
fn the_browser_tags_sessions_and_resumes_the_chosen_one() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let home = mock_home();
    ok(&run(
        home.path(),
        &["-z", "finished one", "--source", "demo"],
    ));
    let slow = home.path().join("slow.toml");
    std::fs::write(
        &slow,
        "[[rule]]\nrepeat = true\nlatency_ms = 20000\ntext = \"slow\"\n",
    )
    .unwrap();
    let mut child = command(home.path())
        .args([
            "-z",
            "cut short",
            "--source",
            "demo",
            "--mock-script",
            slow.to_str().unwrap(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    kill(
        Pid::from_raw(i32::try_from(child.id()).unwrap()),
        Signal::SIGINT,
    )
    .unwrap();
    child.wait().unwrap();
    ok(&run(
        home.path(),
        &[
            "sessions",
            "rename",
            &newest_session(home.path()),
            "Interrupted work",
        ],
    ));

    let refused = run(home.path(), &["sessions", "browse"]);
    assert_eq!(refused.status.code(), Some(2));

    let mut p = spawn(home.path(), &["sessions", "browse"]);
    p.expect("[intr ] Interrupted work").unwrap();
    p.expect("[done ]").unwrap();
    p.expect("Enter to leave").unwrap();
    p.send_line("interrupted").unwrap();
    p.expect("sessions matching 'interrupted'").unwrap();
    p.expect("1. [intr ] Interrupted work").unwrap();
    p.expect("Enter to leave").unwrap();
    p.send_line("1").unwrap();
    p.expect("cut short").unwrap();
    prompt(&mut p);
    exit(p);
}
