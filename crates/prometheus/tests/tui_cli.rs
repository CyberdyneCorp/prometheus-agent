//! `prometheus tui` and the TUI side of `prometheus` on a terminal (spec:
//! tui, TUI launch selection; TUI attaches to the gateway daemon as a
//! client; Standalone in-process mode; Terminal state is always restored;
//! In-TUI update hands off without a relaunch protocol; Slash command
//! flow; Configurable key bindings). The binary runs in a pseudo-terminal;
//! what it drew is read back from the terminal stream.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use common::{daemon, mock_home, ok, run, stderr};
use expectrl::session::OsSession;
use expectrl::{ControlCode, Eof, Expect};

/// Entering and leaving the alternate screen.
const ENTER: &str = "\u{1b}[?1049h";
const LEAVE: &str = "\u{1b}[?1049l";

/// The binary on a terminal with `ui.interface` at its default (`tui`).
fn tui(home: &Path, args: &[&str], env: &[(&str, &str)]) -> OsSession {
    let mut cmd: Command = common::command(home);
    cmd.args(args)
        .current_dir(home)
        .env("TERM", "xterm-256color")
        .env_remove("NO_COLOR");
    for (key, value) in env {
        cmd.env(key, value);
    }
    let mut session = OsSession::spawn(cmd).unwrap();
    session.get_process_mut().set_window_size(100, 30).unwrap();
    session.set_expect_timeout(Some(Duration::from_secs(30)));
    session
}

/// Everything the process writes until it exits, and its exit status.
fn finish(mut p: OsSession) -> (String, Option<i32>) {
    let found = p.expect(Eof).unwrap();
    // Eof matches everything left in the buffer.
    let text = String::from_utf8_lossy(found.as_bytes()).into_owned();
    let status = format!("{:?}", p.get_process_mut().wait().unwrap());
    // `Exited(Pid(n), code)`; a signal death has no exit code.
    let code = status
        .strip_prefix("Exited(")
        .and_then(|rest| rest.trim_end_matches(')').rsplit(", ").next())
        .and_then(|code| code.parse().ok());
    (text, code)
}

/// Waits until the screen shows `text`: its words in order, with only
/// cursor moves or blanks between them (the TUI redraws only the cells
/// that change, so the spaces of a line are not always written).
fn see(p: &mut OsSession, text: &str) {
    let escape = |word: &str| {
        word.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_string()
                } else {
                    format!("\\x{{{:x}}}", u32::from(c))
                }
            })
            .collect::<String>()
    };
    let gap = r"(?:\x1b\[[0-9;?]*[A-Za-z]| )*";
    let pattern = text
        .split_whitespace()
        .map(escape)
        .collect::<Vec<_>>()
        .join(gap);
    p.expect(expectrl::Regex(pattern)).unwrap();
}

/// Whether the terminal behind `handle` is cooked: line editing and
/// signal keys on (raw mode turns both off; echo is as the pseudo-terminal
/// started).
fn cooked(handle: &std::fs::File) -> bool {
    use nix::sys::termios::{LocalFlags, tcgetattr};
    let attrs = tcgetattr(handle).unwrap();
    attrs
        .local_flags
        .contains(LocalFlags::ICANON | LocalFlags::ISIG)
}

/// Types `text` and Enter (a carriage return, as a terminal sends it).
fn enter(p: &mut OsSession, text: &str) {
    p.send(format!("{text}\r")).unwrap();
}

/// Waits for the TUI's composer on the alternate screen.
fn opened(p: &mut OsSession) {
    p.expect(ENTER).unwrap();
    p.expect("default").unwrap();
}

/// Scenario: Piped invocation never opens the TUI.
#[test]
fn a_piped_chat_uses_the_line_mode_and_never_the_alternate_screen() {
    let home = mock_home();
    let out_file = home.path().join("out.txt");
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(format!(
        "'{}' --home '{}' chat --query hello > '{}'",
        common::BIN,
        home.path().display(),
        out_file.display()
    ));
    common::isolate_user_home(&mut cmd, home.path());
    cmd.env("TERM", "xterm-256color");
    let mut p = OsSession::spawn(cmd).unwrap();
    p.set_expect_timeout(Some(Duration::from_secs(30)));
    let (screen, code) = finish(p);
    assert_eq!(code, Some(0), "{screen}");
    let written = std::fs::read_to_string(&out_file).unwrap();
    assert!(written.contains("mock: hello"), "{written}");
    for text in [&screen, &written] {
        assert!(!text.contains(ENTER), "the alternate screen was entered");
    }
}

/// Scenario: Forced TUI without a terminal.
#[test]
fn prometheus_tui_without_a_terminal_exits_2() {
    let home = mock_home();
    let out = run(home.path(), &["tui"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("prometheus tui needs an interactive terminal (stdin is not a TTY)"),
        "{}",
        stderr(&out)
    );
}

/// Task 23.1: `prometheus` on a TTY with `ui.interface = "tui"` attaches
/// to the running daemon as a TUI client.
#[test]
fn prometheus_on_a_terminal_opens_the_tui_attached_to_the_daemon() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    enter(&mut p, "hello tui");
    see(&mut p, "mock: hello tui");
    let status: serde_json::Value =
        serde_json::from_str(&ok(&run(home.path(), &["--json", "daemon", "status"]))).unwrap();
    assert_eq!(status["daemon"]["connections"], 1, "{status}");
    p.send(ControlCode::EndOfTransmission).unwrap();
    let (screen, code) = finish(p);
    assert_eq!(code, Some(0), "{screen}");
    assert!(screen.contains(LEAVE), "left the alternate screen");
    let listed: serde_json::Value = serde_json::from_str(&ok(&run(
        home.path(),
        &["--json", "sessions", "list", "--all"],
    )))
    .unwrap();
    assert_eq!(listed["sessions"][0]["source"], "tui", "{listed}");
}

/// Scenario: First launch starts the daemon.
#[test]
fn the_first_tui_starts_the_daemon_detached_and_attaches() {
    let home = mock_home();
    let mut p = tui(home.path(), &["tui"], &[]);
    opened(&mut p);
    let record = daemon::record(home.path());
    let _daemon = daemon::Daemon {
        home: home.path(),
        pid: record.process.pid,
    };
    assert_ne!(
        record.process.pid,
        p.get_process().pid().as_raw().unsigned_abs()
    );
    enter(&mut p, "first");
    see(&mut p, "mock: first");
    enter(&mut p, "/quit");
    let (_, code) = finish(p);
    assert_eq!(code, Some(0));
    let status = ok(&run(home.path(), &["daemon", "status"]));
    assert!(
        status.starts_with("gateway running"),
        "it outlives the TUI: {status}"
    );
}

/// Scenario: Autostart disabled.
#[test]
fn without_autostart_the_tui_exits_69_and_opens_no_store() {
    let home = mock_home();
    std::fs::write(
        home.path().join("daemon.toml"),
        "[daemon]\nautostart = false\n",
    )
    .unwrap();
    let db = home.path().join("profiles/default/state.db");
    let existed = db.exists();
    let p = tui(home.path(), &["tui"], &[]);
    let (screen, code) = finish(p);
    assert_eq!(code, Some(69), "{screen}");
    assert!(
        screen.contains("gateway daemon is not running; start it with 'prometheus daemon start'"),
        "{screen}"
    );
    assert_eq!(db.exists(), existed, "state.db untouched");
    assert!(!screen.contains(ENTER));
}

/// Scenario: Standalone beside a live daemon.
#[test]
fn standalone_beside_a_running_daemon_exits_75() {
    let home = mock_home();
    let daemon = daemon::start(home.path(), &[]);
    let p = tui(home.path(), &["tui", "--standalone"], &[]);
    let (screen, code) = finish(p);
    assert_eq!(code, Some(75), "{screen}");
    let expected = format!(
        "{} is served by a running gateway daemon (pid {}); run without --standalone to attach",
        home.path().display(),
        daemon.pid
    );
    assert!(
        screen.replace("\r\n", "").contains(&expected) || screen.contains("--standalone"),
        "{screen}"
    );
    assert!(!screen.contains(ENTER));
}

/// Standalone mode: an in-process gateway that serves only this TUI and
/// ends with it.
#[test]
fn standalone_serves_only_its_own_tui_and_ends_with_it() {
    let home = mock_home();
    let mut p = tui(home.path(), &["tui", "--standalone"], &[]);
    opened(&mut p);
    enter(&mut p, "in process");
    see(&mut p, "mock: in process");
    let other = run(home.path(), &["rpc", "call", "session.list"]);
    assert_ne!(
        other.status.code(),
        Some(0),
        "another client gets no ticket"
    );
    assert!(
        stderr(&other).contains("serves only the TUI"),
        "{}",
        stderr(&other)
    );
    enter(&mut p, "/quit");
    let (_, code) = finish(p);
    assert_eq!(code, Some(0));
    assert_eq!(
        ok(&run(home.path(), &["daemon", "status"])).trim(),
        "gateway is not running"
    );
}

/// Task 23.2: SIGTERM restores the terminal mode.
#[test]
fn sigterm_restores_the_terminal_and_exits_143() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    let raw = p.get_process().get_raw_handle().unwrap();
    assert!(!cooked(&raw), "raw mode while the TUI runs");
    let pid = nix::unistd::Pid::from_raw(p.get_process().pid().as_raw());
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).unwrap();
    let (screen, code) = finish(p);
    assert_eq!(code, Some(143), "{screen}");
    assert!(screen.contains(LEAVE), "left the alternate screen");
    assert!(screen.contains("\u{1b}[?25h"), "the cursor is shown");
    assert!(screen.contains("\u{1b}[?2004l"), "bracketed paste is off");
    assert!(cooked(&raw), "cooked mode again");
}

/// Scenario: Panic during render.
#[test]
fn a_panic_while_drawing_restores_the_terminal_reports_and_exits_70() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let p = tui(home.path(), &[], &[("PMT_TEST_TUI_PANIC", "1")]);
    let raw = p.get_process().get_raw_handle().unwrap();
    let (screen, code) = finish(p);
    assert_eq!(code, Some(70), "{screen}");
    let left = screen.find(LEAVE).expect("left the alternate screen");
    let said = screen
        .find("prometheus tui crashed; the panic report is in ")
        .expect("the report path on stderr");
    assert!(
        left < said,
        "the terminal was restored before the report path"
    );
    let logs = home.path().join("logs");
    let report = std::fs::read_dir(&logs)
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().starts_with("tui-panic-"))
        .expect("tui-panic-<timestamp>.log");
    let text = std::fs::read_to_string(report.path()).unwrap();
    assert!(text.contains("PMT_TEST_TUI_PANIC"), "{text}");
    assert!(cooked(&raw), "cooked mode again");
}

/// Scenario: Update requested from the TUI.
#[test]
fn update_restores_the_terminal_before_the_updater_and_exits_with_its_code() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    enter(&mut p, "/update");
    let (screen, code) = finish(p);
    // A source build is owned by `dev`: `prometheus update` refuses (69).
    let direct = run(home.path(), &["update"]);
    assert_eq!(code, direct.status.code(), "{screen}");
    assert_eq!(code, Some(69));
    let left = screen.rfind(LEAVE).expect("left the alternate screen");
    let said = screen.find("owner: dev").expect("the updater's message");
    assert!(left < said, "restored before the updater printed");
}

/// Scenario: A path is not a command. Scenario: Skill command.
#[test]
fn a_skill_command_is_dispatched_and_a_path_is_a_prompt() {
    let home = mock_home();
    let dir = home.path().join("profiles/default/skills/deploy");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: deploy\ndescription: Deploys.\n---\n\nDEPLOY-STEPS\n",
    )
    .unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    enter(&mut p, "/var/log/syslog is huge");
    see(&mut p, "mock: /var/log/syslog is huge");
    enter(&mut p, "/deploy staging");
    p.expect("staging").unwrap();
    enter(&mut p, "/quit");
    let (_, code) = finish(p);
    assert_eq!(code, Some(0));
    let id = common::newest_session(home.path());
    let messages = common::session_messages(home.path(), &id);
    let users: Vec<&str> = messages
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert_eq!(users[0], "/var/log/syslog is huge");
    assert!(
        users[1].contains("DEPLOY-STEPS") && users[1].contains("staging"),
        "the skill's message was submitted as a prompt: {users:?}"
    );
}

/// Scenario: Unknown action.
#[test]
fn an_unknown_key_action_refuses_to_start_naming_the_key() {
    let home = mock_home();
    common::append_config(home.path(), "\n[ui.tui.keys]\nscrool_up = \"PageUp\"\n");
    let p = tui(home.path(), &["tui"], &[]);
    let (screen, code) = finish(p);
    assert_eq!(code, Some(78), "{screen}");
    assert!(screen.contains("ui.tui.keys.scrool_up"), "{screen}");
    assert!(!screen.contains(ENTER));
    let bad_chord = mock_home();
    common::append_config(bad_chord.path(), "\n[ui.tui.keys]\npage_up = \"Ctl+B\"\n");
    let (screen, code) = finish(tui(bad_chord.path(), &["tui"], &[]));
    assert_eq!(code, Some(78), "{screen}");
    assert!(screen.contains("ui.tui.keys.page_up"), "{screen}");
}

/// `/keys` lists the effective bindings, a configured one included.
#[test]
fn keys_lists_the_effective_bindings() {
    let home = mock_home();
    common::append_config(home.path(), "\n[ui.tui.keys]\npage_up = \"Ctrl+B\"\n");
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    enter(&mut p, "/keys");
    p.expect("Ctrl+B").unwrap();
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
}

/// Scenario: Reconnect after a daemon restart (the TUI side): the TUI
/// re-attaches to the restarted daemon from its watermark, is told the
/// replay is truncated (another epoch), reloads the transcript and keeps
/// driving the same session.
#[test]
fn after_a_daemon_restart_the_tui_reattaches_to_the_same_session() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &[], &[]);
    opened(&mut p);
    enter(&mut p, "before restart");
    see(&mut p, "mock: before restart");
    let session = common::newest_session(home.path());
    ok(&run(home.path(), &["daemon", "restart"]));
    see(&mut p, "reconnecting");
    // Back once the note is gone from the status line.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        let status = ok(&run(home.path(), &["--json", "daemon", "status"]));
        if status.contains("\"connections\": 1") || status.contains("\"connections\":1") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    enter(&mut p, "after restart");
    see(&mut p, "mock: after restart");
    enter(&mut p, "/quit");
    assert_eq!(finish(p).1, Some(0));
    assert_eq!(
        common::newest_session(home.path()),
        session,
        "the same session"
    );
    let texts: Vec<String> = common::session_messages(home.path(), &session)
        .iter()
        .filter_map(|m| m["content"].as_str().map(str::to_owned))
        .collect();
    assert!(texts.contains(&"after restart".to_owned()), "{texts:?}");
}
