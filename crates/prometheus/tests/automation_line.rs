//! Session automation in the line interface, driven through a
//! pseudo-terminal with the mock provider as both the agent and the
//! `goal_judge` route (spec: session-automation: Persisted per-session
//! automation state; Idle-boundary arbitration; Goal command surface; Goal
//! judge loop; Goal turn budget and blocked verdict; Completion contracts;
//! Quality gates; Goal wait barriers; Loop command and cadence; Loop stop
//! conditions; Automation is distinct from kanban and cron).

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use common::{ok, run};
use expectrl::session::OsSession;
use expectrl::{Eof, Expect};

/// The judge's prompt starts with this; the agent's turns never do.
const JUDGE: &str = "You are the goal judge";

/// A mock home whose script answers the judge with `judge` rules and
/// every other request with `worked on it`.
fn home_with(judge: &str) -> tempfile::TempDir {
    let home = common::mock_home();
    let script = format!("{judge}\n[[rule]]\nrepeat = true\ntext = \"worked on it\"\n");
    std::fs::write(home.path().join("script.toml"), script).unwrap();
    home
}

fn spawn(home: &Path, args: &[&str]) -> OsSession {
    let script = home.join("script.toml");
    let mut cmd: Command = common::command(home);
    cmd.args(["--set", "ui.interface=line"])
        .arg("--mock-script")
        .arg(&script)
        .args(args)
        .current_dir(home)
        .env("TERM", "xterm")
        .env_remove("NO_COLOR");
    let mut session = OsSession::spawn(cmd).unwrap();
    session.set_expect_timeout(Some(Duration::from_mins(1)));
    session
}

fn prompt(p: &mut OsSession) {
    p.expect("› ").unwrap();
}

fn exit(mut p: OsSession) {
    p.send_line("/exit").unwrap();
    p.expect(Eof).unwrap();
    let status = p.get_process_mut().wait().unwrap();
    assert!(format!("{status:?}").ends_with(", 0)"), "{status:?}");
}

fn judge_says(times: Option<u32>, verdict: &str) -> String {
    let uses = times.map_or_else(|| "repeat = true".to_owned(), |n| format!("times = {n}"));
    format!("[[rule]]\nuser = \"{JUDGE}\"\n{uses}\ntext = '{verdict}'\n")
}

/// The user messages of the newest session as `(origin, text)`.
fn user_turns(home: &Path) -> Vec<(Option<String>, String)> {
    common::session_messages(home, &common::newest_session(home))
        .into_iter()
        .filter(|m| m["role"] == "user")
        .map(|m| {
            (
                m["metadata"]["origin"].as_str().map(str::to_owned),
                m["content"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

/// Scenarios: Continue injects the next turn; Goal does not touch the
/// board; Cache prefix preserved (the continuation is a plain user turn
/// stored with `origin = goal`).
#[test]
fn a_goal_continues_on_the_judges_word_until_it_is_achieved() {
    let judge = format!(
        "{}{}",
        judge_says(
            Some(1),
            r#"{"verdict":"continue","reason":"3 files remain"}"#
        ),
        judge_says(None, r#"{"verdict":"done","reason":"all ported"}"#)
    );
    let home = home_with(&judge);
    let board = || {
        let dir = home.path().join("kanban");
        if dir.exists() {
            common::files_under(&dir)
        } else {
            Vec::new()
        }
    };
    let before = board();
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("/goal port the parser").unwrap();
    p.expect("Goal set: port the parser").unwrap();
    p.expect("Continuing toward goal (1/20): 3 files remain")
        .unwrap();
    p.expect("Goal achieved: all ported").unwrap();
    prompt(&mut p);
    p.send_line("/goal status").unwrap();
    p.expect("status: done · 1/20 turns used").unwrap();
    exit(p);
    let turns = user_turns(home.path());
    assert_eq!(turns.len(), 2, "{turns:?}");
    assert_eq!(turns[0].0.as_deref(), Some("goal"));
    assert_eq!(turns[0].1, "port the parser");
    assert_eq!(turns[1].0.as_deref(), Some("goal"));
    assert!(turns[1].1.starts_with("[goal continuation]"), "{turns:?}");
    // Neither a scheduled job nor a kanban task came of it.
    let jobs = ok(&run(home.path(), &["--json", "cron", "list"]));
    let jobs: serde_json::Value = serde_json::from_str(&jobs).unwrap();
    assert_eq!(jobs["jobs"].as_array().map(Vec::len), Some(0), "{jobs}");
    assert_eq!(board(), before);
}

/// Scenarios: Goal survives resume; Dead pid rejected.
#[test]
fn a_goal_survives_resume_with_its_turn_count() {
    let judge = format!(
        "{}{}",
        judge_says(Some(4), r#"{"verdict":"continue","reason":"more"}"#),
        judge_says(
            None,
            r#"{"verdict":"wait","wait_for_seconds":3600,"reason":"CI"}"#
        )
    );
    let home = home_with(&judge);
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("/goal ship the release").unwrap();
    p.expect("Goal parked on a timer").unwrap();
    prompt(&mut p);
    p.send_line("/goal wait 99999").unwrap();
    p.expect("process 99999 is not running").unwrap();
    exit(p);
    let id = common::newest_session(home.path());
    let mut p = spawn(home.path(), &["--resume", &id]);
    prompt(&mut p);
    p.send_line("/goal status").unwrap();
    p.expect("Goal: ship the release").unwrap();
    p.expect("status: active · 4/20 turns used").unwrap();
    exit(p);
}

/// Scenarios: Budget exhaustion pauses; Broken judge fails open.
#[test]
fn a_failing_judge_keeps_going_until_the_budget_pauses_the_goal() {
    let judge = format!(
        "[[rule]]\nuser = \"{JUDGE}\"\nrepeat = true\nfault = \"server_error\"\nstatus = 500\n"
    );
    let home = home_with(&judge);
    let mut p = spawn(home.path(), &["--set", "goals.max_turns=3"]);
    prompt(&mut p);
    p.send_line("/goal keep polishing").unwrap();
    p.expect("Continuing toward goal (1/3)").unwrap();
    p.expect("Continuing toward goal (2/3)").unwrap();
    p.expect("Continuing toward goal (3/3)").unwrap();
    p.expect(
        "Goal paused — 3/3 turns used. Use /goal resume to keep going, or /goal clear to stop.",
    )
    .unwrap();
    prompt(&mut p);
    p.send_line("/goal status").unwrap();
    p.expect("status: paused · 3/3 turns used").unwrap();
    exit(p);
    let continuations = user_turns(home.path())
        .iter()
        .filter(|(_, text)| text.starts_with("[goal continuation]"))
        .count();
    assert_eq!(continuations, 3);
}

/// Scenarios: Unachievable goal pauses; Draft falls back (the judge
/// route fails, the objective becomes the goal and its first turn runs).
#[test]
fn a_blocked_goal_pauses_and_a_failed_draft_sets_the_objective() {
    let blocked = judge_says(
        Some(1),
        r#"{"verdict":"blocked","reason":"requires production DB credentials"}"#,
    );
    let judge = format!(
        "{blocked}[[rule]]\nuser = \"{JUDGE}\"\nrepeat = true\nfault = \"server_error\"\nstatus = 500\n"
    );
    let home = home_with(&judge);
    let mut p = spawn(home.path(), &["--set", "goals.max_turns=1"]);
    prompt(&mut p);
    p.send_line("/goal migrate the database").unwrap();
    p.expect("Goal judged unachievable — paused: requires production DB credentials")
        .unwrap();
    prompt(&mut p);
    p.send_line("/goal status").unwrap();
    p.expect("status: paused").unwrap();
    prompt(&mut p);
    p.send_line("/goal draft migrate auth to JWT").unwrap();
    p.expect("Goal set: migrate auth to JWT").unwrap();
    p.expect("Goal paused — 1/1 turns used").unwrap();
    prompt(&mut p);
    p.send_line("/goal show").unwrap();
    p.expect("(no completion contract)").unwrap();
    exit(p);
    let turns = user_turns(home.path());
    assert!(
        turns
            .iter()
            .any(|(o, t)| o.as_deref() == Some("goal") && t == "migrate auth to JWT"),
        "{turns:?}"
    );
}

/// Scenario: User input preempts continuation.
#[test]
fn a_line_typed_during_the_turn_runs_before_the_continuation() {
    let judge = format!(
        "{}{}",
        judge_says(Some(1), r#"{"verdict":"continue","reason":"keep at it"}"#),
        judge_says(None, r#"{"verdict":"done","reason":"ok"}"#)
    );
    let home = common::mock_home();
    let script = format!(
        "{judge}\n[[rule]]\nuser = \"port the parser\"\nlatency_ms = 1500\ntext = \"started\"\n\n\
         [[rule]]\nrepeat = true\ntext = \"worked on it\"\n"
    );
    std::fs::write(home.path().join("script.toml"), script).unwrap();
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("/goal port the parser").unwrap();
    common::pty::wait_for_turns(&mut p, home.path(), 1);
    p.send_line("also keep the old API").unwrap();
    p.expect("queued").unwrap();
    p.expect("Goal achieved: ok").unwrap();
    prompt(&mut p);
    exit(p);
    let turns = user_turns(home.path());
    let texts: Vec<&str> = turns.iter().map(|(_, t)| t.as_str()).collect();
    assert_eq!(texts[0], "port the parser");
    assert_eq!(texts[1], "also keep the old API", "{texts:?}");
    assert_eq!(turns[1].0, None);
    // The judge looked again after the person's turn and was satisfied:
    // the continuation it first asked for never ran.
    assert_eq!(texts.len(), 2, "{texts:?}");
}

/// Scenario: Red gate blocks completion (the judge would say done).
#[test]
fn a_red_quality_gate_skips_the_judge() {
    let judge = judge_says(None, r#"{"verdict":"done","reason":"looks done"}"#);
    let home = common::mock_home();
    let script = format!(
        "{judge}\n[[rule]]\nuser = \"fix the build\"\nlatency_ms = 2000\ntext = \"on it\"\n\n\
         [[rule]]\nrepeat = true\ntext = \"worked on it\"\n"
    );
    std::fs::write(home.path().join("script.toml"), script).unwrap();
    let mut p = spawn(home.path(), &["--set", "goals.max_turns=1"]);
    prompt(&mut p);
    p.send_line("/goal fix the build").unwrap();
    common::pty::wait_for_turns(&mut p, home.path(), 1);
    p.send_line("/goal gate add echo broken && exit 101")
        .unwrap();
    p.expect("Quality gate 1 added").unwrap();
    p.expect("Continuing toward goal (1/1): quality gate 1 failed (exit 101)")
        .unwrap();
    p.expect("Goal paused").unwrap();
    prompt(&mut p);
    exit(p);
    let turns = user_turns(home.path());
    let continuation = &turns[1].1;
    assert!(continuation.contains("exit 101"), "{turns:?}");
    assert!(continuation.contains("broken"), "{turns:?}");
}

/// Scenarios: Interval below the floor; Run cap; Interrupt pauses.
#[test]
fn loops_refuse_short_intervals_stop_at_their_cap_and_pause_on_ctrl_c() {
    let home = common::mock_home();
    std::fs::write(
        home.path().join("script.toml"),
        "[[rule]]\nuser = \"slow watch\"\nrepeat = true\nlatency_ms = 5000\ntext = \"watched\"\n\n\
         [[rule]]\nrepeat = true\ntext = \"pending\"\n",
    )
    .unwrap();
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("/loop 10s check the queue").unwrap();
    p.expect("interval must be at least 30s (loops.min_interval_secs)")
        .unwrap();
    exit(p);
    let mut p = spawn(home.path(), &["--set", "loops.min_interval_secs=1"]);
    prompt(&mut p);
    p.send_line("/loop 1s poll CI --times 3").unwrap();
    p.expect("Loop finished after 3 runs.").unwrap();
    prompt(&mut p);
    p.send_line("/loop status").unwrap();
    p.expect("No loop.").unwrap();
    prompt(&mut p);
    p.send_line("/loop 30s slow watch").unwrap();
    common::pty::wait_for_turns(&mut p, home.path(), 4);
    // Well inside the 5 s wakeup turn; the terminal is read meanwhile (a
    // full pseudo-terminal would block the interface's own output).
    for _ in 0..10 {
        let _ = p.check(expectrl::Regex("(?s).+"));
        std::thread::sleep(Duration::from_millis(100));
    }
    p.send("\x03").unwrap();
    p.expect("Loop paused: the wakeup turn was interrupted")
        .unwrap();
    prompt(&mut p);
    p.send_line("/loop status").unwrap();
    p.expect("paused").unwrap();
    exit(p);
    let loops = user_turns(home.path())
        .iter()
        .filter(|(o, _)| o.as_deref() == Some("loop"))
        .count();
    assert_eq!(loops, 4);
}

/// `--until` (spec: Loop stop conditions): the judge is asked after each
/// wakeup; an unreadable verdict keeps the loop going, yes stops it, and an
/// unachievable condition pauses it with the reason.
#[test]
fn a_loop_until_a_condition_stops_on_yes_and_pauses_when_unachievable() {
    let judge = format!(
        "{}{}{}{}",
        judge_says(Some(1), "not a verdict"),
        judge_says(Some(1), r#"{"verdict":"continue","reason":"still red"}"#),
        judge_says(
            Some(1),
            r#"{"verdict":"done","reason":"the deploy is green"}"#
        ),
        judge_says(
            None,
            r#"{"verdict":"blocked","reason":"the branch was deleted"}"#
        ),
    );
    let home = home_with(&judge);
    let mut p = spawn(home.path(), &["--set", "loops.min_interval_secs=1"]);
    prompt(&mut p);
    p.send_line("/loop 1s poll the deploy --until the deploy is green")
        .unwrap();
    p.expect("Loop condition met: the deploy is green").unwrap();
    prompt(&mut p);
    p.send_line("/loop status").unwrap();
    p.expect("No loop.").unwrap();
    let polls = user_turns(home.path())
        .iter()
        .filter(|(o, _)| o.as_deref() == Some("loop"))
        .count();
    assert_eq!(
        polls, 3,
        "an unreadable and an undecided verdict kept it going"
    );
    prompt(&mut p);
    p.send_line("/loop 1s check the branch --until the branch is merged")
        .unwrap();
    p.expect("Loop paused: judged unachievable: the branch was deleted")
        .unwrap();
    prompt(&mut p);
    p.send_line("/loop status").unwrap();
    p.expect("paused").unwrap();
    exit(p);
    let turns = user_turns(home.path());
    let loops: Vec<&str> = turns
        .iter()
        .filter(|(o, _)| o.as_deref() == Some("loop"))
        .map(|(_, t)| t.as_str())
        .collect();
    assert_eq!(loops.len(), 4, "one wakeup of the paused loop: {loops:?}");
    assert!(loops[3].contains("check the branch"), "{loops:?}");
}
