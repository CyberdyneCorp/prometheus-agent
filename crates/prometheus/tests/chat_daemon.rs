//! `prometheus chat` with a running gateway daemon: the chat is a client
//! of the daemon, which owns the session store (spec: cli, Daemon
//! attachment with in-process fallback; Single interactive owner per
//! session; workspace-architecture, Clients attach to a running daemon).

#![cfg(feature = "gateway")]

mod common;

use serde_json::Value;

#[cfg(unix)]
use self::common::pty;
use self::common::{daemon, mock_home, ok, run, stderr};
#[cfg(unix)]
use expectrl::Expect as _;

fn json_of(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

/// The ids `prometheus sessions list --all` shows.
fn listed_sessions(home: &std::path::Path) -> Vec<String> {
    let listed = json_of(&ok(&run(home, &["--json", "sessions", "list", "--all"])));
    listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_owned())
        .collect()
}

/// Scenario: Chat attaches to a running daemon. Scenario: Session shared
/// with gateway. The daemon holds the home's writer lock, so a chat that
/// opened `state.db` itself would exit 75; this one runs its turn in the
/// daemon and the session is the daemon's.
#[test]
fn a_chat_runs_as_a_client_of_the_running_daemon() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let out = run(home.path(), &["--json", "-z", "hello daemon"]);
    let report = json_of(&ok(&out));
    assert_eq!(report["answer"], "mock: hello daemon");
    assert_eq!(report["exit_reason"], "completed");
    let session = report["session_id"].as_str().unwrap();
    assert!(listed_sessions(home.path()).contains(&session.to_owned()));
    let transcript = json_of(&ok(&run(
        home.path(),
        &[
            "rpc",
            "call",
            "session.transcript",
            &format!("{{\"session_id\": \"{session}\"}}"),
        ],
    )));
    assert_eq!(transcript["messages"][1]["text"], "mock: hello daemon");
    let in_process = run(home.path(), &["--in-process", "-z", "hi"]);
    assert_eq!(
        in_process.status.code(),
        Some(75),
        "{}",
        stderr(&in_process)
    );
}

#[test]
fn flags_the_daemon_cannot_honour_are_refused_by_name() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let out = run(home.path(), &["-z", "hi", "--yolo"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("--yolo cannot be used while the gateway daemon"),
        "{}",
        stderr(&out)
    );
    let out = run(
        home.path(),
        &["--set", "agent.max_iterations=3", "-z", "hi"],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

#[test]
fn a_profile_the_daemon_does_not_serve_cannot_chat_while_it_runs() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    ok(&run(home.path(), &["profile", "create", "late"]));
    let out = run(home.path(), &["-p", "late", "-z", "hi"]);
    assert_eq!(out.status.code(), Some(75), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("does not serve profile 'late'"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn resume_and_continue_are_resolved_by_the_daemon() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let first = json_of(&ok(&run(home.path(), &["--json", "-z", "one"])));
    let session = first["session_id"].as_str().unwrap();
    let again = json_of(&ok(&run(
        home.path(),
        &["--json", "-r", &session[..12], "-z", "two"],
    )));
    assert_eq!(again["session_id"], session);
    let missing = run(home.path(), &["-r", "no-such-title", "-z", "x"]);
    assert_eq!(missing.status.code(), Some(66), "{}", stderr(&missing));
}

/// The line interface on a terminal attaches and runs turns in the
/// daemon.
#[cfg(unix)]
#[test]
fn the_line_interface_attaches_to_the_daemon() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = pty::spawn(home.path(), &[]);
    p.expect("attached to the gateway daemon").unwrap();
    pty::prompt(&mut p);
    p.send_line("hello there").unwrap();
    p.expect("mock: hello there").unwrap();
    pty::prompt(&mut p);
    p.send_line("/model").unwrap();
    p.expect("not available while attached").unwrap();
    pty::exit(p);
}

/// Ctrl-C at the attached interface interrupts the daemon's turn.
#[cfg(unix)]
#[test]
fn ctrl_c_interrupts_the_turn_running_in_the_daemon() {
    let home = mock_home();
    let script = home.path().join("slow.toml");
    std::fs::write(
        &script,
        "[[rule]]\nrepeat = true\nlatency_ms = 20000\ntext = \"slow\"\n",
    )
    .unwrap();
    common::append_config(
        home.path(),
        &format!(
            "\n[providers.mock]\nscript = {:?}\n",
            script.display().to_string()
        ),
    );
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = pty::spawn(home.path(), &[]);
    pty::prompt(&mut p);
    p.send_line("take your time").unwrap();
    common::wait_for_turn_start(home.path());
    let sent = std::time::Instant::now();
    p.send("\x03").unwrap();
    p.expect("(interrupted)").unwrap();
    assert!(sent.elapsed() < std::time::Duration::from_secs(10));
    pty::prompt(&mut p);
    pty::exit(p);
}

#[cfg(unix)]
fn session_of_new_chat(home: &std::path::Path) -> String {
    let report: serde_json::Value =
        serde_json::from_str(&ok(&run(home, &["--json", "-z", "first"]))).unwrap();
    report["session_id"].as_str().unwrap().to_owned()
}

/// Two CLIs on one session: the second gets the ownership error naming
/// the first one's pid.
#[cfg(unix)]
#[test]
fn a_second_cli_on_an_owned_session_gets_an_ownership_error() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let session = session_of_new_chat(home.path());
    let mut owner = pty::spawn(home.path(), &["-r", &session]);
    owner.expect("attached to the gateway daemon").unwrap();
    pty::prompt(&mut owner);
    let pid = owner.get_process().pid();
    let out = run(home.path(), &["-r", &session, "-z", "me too"]);
    assert_eq!(out.status.code(), Some(75), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!(
            "session {session} is open in another client (pid {pid})"
        )),
        "{}",
        stderr(&out)
    );
    pty::exit(owner);
    ok(&run(home.path(), &["-r", &session, "-z", "now mine"]));
}

/// Scenario: Second terminal offered a fork.
#[cfg(unix)]
#[test]
fn a_second_terminal_forks_the_owned_session_at_its_last_message() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let session = session_of_new_chat(home.path());
    let mut owner = pty::spawn(home.path(), &["-r", &session]);
    owner.expect("attached to the gateway daemon").unwrap();
    pty::prompt(&mut owner);

    let mut second = pty::spawn(home.path(), &["-r", &session]);
    second.expect("is open in another client (pid").unwrap();
    second.expect("fork it [k]").unwrap();
    second.send_line("k").unwrap();
    let forked = second
        .expect(expectrl::Regex(r"forked session \S+ as (ses_[0-9A-Z]+)"))
        .unwrap();
    let fork = String::from_utf8(forked.get(1).unwrap().to_vec()).unwrap();
    assert_ne!(fork, session);
    second.expect("attached to the gateway daemon").unwrap();
    pty::prompt(&mut second);
    second.send_line("in the fork").unwrap();
    second.expect("mock: in the fork").unwrap();
    pty::prompt(&mut second);

    let transcript: serde_json::Value = serde_json::from_str(&ok(&run(
        home.path(),
        &[
            "rpc",
            "call",
            "session.transcript",
            &format!("{{\"session_id\": \"{fork}\"}}"),
        ],
    )))
    .unwrap();
    let texts: Vec<&str> = transcript["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["text"].as_str())
        .collect();
    assert_eq!(texts[..2], ["first", "mock: first"], "{texts:?}");
    assert!(texts.contains(&"mock: in the fork"), "{texts:?}");
    pty::exit(second);
    pty::exit(owner);
}
