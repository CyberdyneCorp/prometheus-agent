//! The TUI's live sessions on a pseudo-terminal (spec: tui, Live session
//! switcher; Auto-resume of the last TUI session): a second session starts
//! from the switcher while the first keeps running, and a TUI whose
//! transport was killed comes back to its session on the next launch.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::time::Duration;

use common::tui::{enter, finish, opened, see, see_repainted, tui};
use common::{daemon, mock_home, ok, run};
use expectrl::Expect;

fn script(home: &Path) -> String {
    let path = home.join("busy.toml");
    std::fs::write(
        &path,
        "[[rule]]\nuser = \"slow\"\nrepeat = true\nlatency_ms = 4000\ntext = \"slow done\"\n\n\
         [[rule]]\nrepeat = true\ntext = \"fast done\"\n",
    )
    .unwrap();
    path.to_string_lossy().into_owned()
}

fn sessions(home: &Path) -> Vec<String> {
    let listed = ok(&run(home, &["--json", "sessions", "list", "--all"]));
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_owned())
        .collect()
}

fn texts(home: &Path, id: &str) -> Vec<String> {
    common::session_messages(home, id)
        .iter()
        .filter_map(|m| m["content"].as_str().map(str::to_owned))
        .collect()
}

/// Scenario: Dispatching a second session.
#[test]
fn a_new_session_from_the_switcher_runs_while_the_first_keeps_running() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let script = script(home.path());
    let mut p = tui(home.path(), &["chat", "--mock-script", &script], &[]);
    opened(&mut p);
    enter(&mut p, "slow");
    std::thread::sleep(Duration::from_millis(500));
    let first = common::newest_session(home.path());
    // Ctrl+X opens the switcher; Down to `+new`, the prompt, Enter.
    p.send("\u{18}").unwrap();
    see(&mut p, "+new");
    p.send("\u{1b}[B").unwrap();
    p.send("second prompt").unwrap();
    // The draft's row: `+new` itself may already be on the screen (drawn
    // when Down highlighted it), so only the cells after it are written.
    see(&mut p, "[default model, Tab changes] ›");
    std::thread::sleep(Duration::from_millis(300));
    p.send("\r").unwrap();
    // From here on the composer has the focus, so every check waits for a
    // full repaint: new lines land on cells that held the other session's
    // text and only the differing ones are written.
    see_repainted(&mut p, "fast done");
    see_repainted(&mut p, "2 live");
    // The first session's turn ended in the background meanwhile.
    see_repainted(&mut p, "finished a turn in the background");
    // Back to the first one through the switcher.
    p.send("\u{18}").unwrap();
    see(&mut p, "sessions:");
    p.send("\u{1b}[A").unwrap();
    p.send("\r").unwrap();
    see_repainted(&mut p, &format!("session {first}"));
    see_repainted(&mut p, "slow done");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
    let all = sessions(home.path());
    assert_eq!(all.len(), 2, "{all:?}");
    let second = all.iter().find(|s| **s != first).unwrap();
    assert!(texts(home.path(), &first).contains(&"slow done".to_owned()));
    assert!(texts(home.path(), second).contains(&"second prompt".to_owned()));
}

#[test]
fn ctrl_d_in_the_switcher_closes_a_live_session_and_sessions_new_starts_one() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    enter(&mut p, "/sessions new");
    see(&mut p, "2 live");
    p.send("\u{18}").unwrap();
    see(&mut p, "sessions:");
    // The shown (second) session is highlighted: Ctrl+D closes it.
    p.send("\u{4}").unwrap();
    std::thread::sleep(Duration::from_millis(500));
    p.send("\u{1b}").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    enter(&mut p, "/redraw");
    see(&mut p, "closed session");
    enter(&mut p, "still here");
    see(&mut p, "mock: still here");
    enter(&mut p, "/quit");
    let (text, code) = finish(p);
    assert_eq!(code, Some(0));
    assert!(!text.contains("2 live"));
}

/// Scenario: Reconnecting after a dropped SSH link: the TUI's transport is
/// killed (the TUI dies with its terminal) and the next launch from the
/// same directory re-attaches to the session and shows its transcript.
#[test]
fn after_the_transport_is_killed_the_next_tui_resumes_the_session() {
    let home = mock_home();
    common::append_config(home.path(), "\n[ui.tui]\nauto_resume = \"recent\"\n");
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    enter(&mut p, "remember the api work");
    see(&mut p, "mock: remember the api work");
    let session = common::newest_session(home.path());
    let pid = p.get_process().pid();
    let pid = nix::unistd::Pid::from_raw(pid.as_raw());
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL).unwrap();
    // Read to the end: a dying process waits for its terminal to drain.
    let _ = p.expect(expectrl::Eof);
    let _ = p.get_process_mut().wait();
    // The daemon releases the dead connection's attachment.
    std::thread::sleep(Duration::from_millis(500));
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    see(&mut p, "mock: remember the api work");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
    assert_eq!(sessions(home.path()), [session], "no new session");
    // `--resume` wins over the recent session.
    let other = ok(&run(home.path(), &["--json", "-z", "elsewhere"]));
    let other: serde_json::Value = serde_json::from_str(&other).unwrap();
    let other = other["session_id"].as_str().unwrap().to_owned();
    let mut p = tui(home.path(), &["tui", "--resume", &other], &[]);
    opened(&mut p);
    see(&mut p, "mock: elsewhere");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
}

#[test]
fn a_deleted_recent_session_falls_back_to_a_new_one() {
    let home = mock_home();
    common::append_config(home.path(), "\n[ui.tui]\nauto_resume = \"recent\"\n");
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    enter(&mut p, "short lived");
    see(&mut p, "mock: short lived");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
    let gone = common::newest_session(home.path());
    ok(&run(home.path(), &["sessions", "delete", &gone, "--yes"]));
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    enter(&mut p, "fresh");
    see(&mut p, "mock: fresh");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
    let all = sessions(home.path());
    assert_eq!(all.len(), 1, "{all:?}");
    assert_ne!(all[0], gone);
}

/// The transport killed under a running TUI (a daemon restart): every live
/// session is re-attached, the shown one and the one in the background.
#[test]
fn after_a_daemon_restart_every_live_session_is_reattached() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    enter(&mut p, "first");
    see(&mut p, "mock: first");
    let first = common::newest_session(home.path());
    enter(&mut p, "/sessions new");
    see(&mut p, "2 live");
    ok(&run(home.path(), &["daemon", "restart"]));
    see(&mut p, "reconnecting");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        let status = ok(&run(home.path(), &["--json", "daemon", "status"]));
        if status.contains("\"connections\": 1") || status.contains("\"connections\":1") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Back on the first session, which the TUI still drives.
    p.send("\u{18}").unwrap();
    see(&mut p, "sessions:");
    p.send("\u{1b}[A").unwrap();
    p.send("\r").unwrap();
    see(&mut p, &format!("session {first}"));
    enter(&mut p, "again");
    see(&mut p, "mock: again");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
    assert!(texts(home.path(), &first).contains(&"again".to_owned()));
}
