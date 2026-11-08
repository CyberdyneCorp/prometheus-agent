//! Crash resume mid-tool-batch through the binary: a one-shot chat is
//! killed while its `terminal` call runs, then `prometheus sessions resume`
//! continues the turn from its journal (spec: agent-turn-loop, Crash resume
//! mid-tool-batch; Deterministic turn journal).

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use expectrl::session::OsSession;
use expectrl::{Eof, Expect};

const SCRIPT: &str = r#"
[[rule]]
step = 0
[[rule.tool_calls]]
name = "read_file"
arguments = { path = "a.txt" }
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "touch started && sleep 60 && touch deployed" }

[[rule]]
step = 1
text = "checked the deploy"

[[rule]]
step = 2
text = "nothing else"
"#;

fn home() -> tempfile::TempDir {
    let home = common::mock_home();
    let script = home.path().join("mock.toml");
    std::fs::write(&script, SCRIPT).unwrap();
    common::append_config(
        home.path(),
        &format!(
            "\n[providers.mock]\nscript = {:?}\n",
            script.display().to_string()
        ),
    );
    std::fs::create_dir_all(work(home.path())).unwrap();
    std::fs::write(
        work(home.path()).join("a.txt"),
        "version before the crash\n",
    )
    .unwrap();
    home
}

/// The session's workspace. Not the Prometheus home itself: that holds the
/// isolated user home, whose credential stores the sandbox denies, and
/// Landlock (Linux) can then only list the directory above them, so the
/// command could not create `started` there.
fn work(home: &Path) -> std::path::PathBuf {
    home.join("work")
}

/// Runs the one-shot turn and kills it once `terminal` is running.
fn crash(home: &Path) {
    let mut child: std::process::Child = common::command(home)
        .current_dir(work(home))
        .args(["--json", "--yolo", "-z", "deploy it"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !work(home).join("started").exists() {
        assert!(
            started.elapsed() < Duration::from_mins(1),
            "terminal never ran"
        );
        if child.try_wait().unwrap().is_some() {
            let session = common::newest_session(home);
            let messages = common::session_messages(home, &session);
            panic!("the chat ended early: {messages:#?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.kill().unwrap();
    child.wait().unwrap();
}

fn resume(home: &Path, session: &str) -> OsSession {
    let mut cmd: Command = common::command(home);
    cmd.args([
        "--set",
        "ui.interface=line",
        "--set",
        "agent.resume_on_crash=auto",
    ])
    .args(["sessions", "resume", session])
    .current_dir(work(home))
    .env("TERM", "xterm")
    .env_remove("NO_COLOR");
    let mut p = OsSession::spawn(cmd).unwrap();
    p.set_expect_timeout(Some(Duration::from_mins(1)));
    p
}

/// Waits for `needle`; on a timeout panics with what the screen and the
/// stored transcript hold, so a slow-runner failure says where it stopped.
fn expect_or_dump(p: &mut OsSession, home: &Path, session: &str, needle: &str) {
    if let Err(err) = p.expect(needle) {
        let mut screen = Vec::new();
        let mut buf = [0_u8; 4096];
        while let Ok(n @ 1..) = p.try_read(&mut buf) {
            screen.extend_from_slice(&buf[..n]);
        }
        let messages = common::session_messages(home, session);
        panic!(
            "{needle:?}: {err}\nscreen after the last match:\n{}\ntranscript: {messages:#?}",
            String::from_utf8_lossy(&screen)
        );
    }
}

#[test]
fn a_crash_during_a_terminal_command_resumes_with_the_read_reused_and_the_command_unknown() {
    let home = home();
    crash(home.path());
    let session = common::newest_session(home.path());
    // Changed after the crash: a re-run read would see it.
    std::fs::write(work(home.path()).join("a.txt"), "version after the crash\n").unwrap();

    let mut p = resume(home.path(), &session);
    for needle in [
        "stopped when the process ended while terminal ran",
        "checked the deploy",
        "› ",
    ] {
        expect_or_dump(&mut p, home.path(), &session, needle);
    }
    p.send_line("/exit").unwrap();
    p.expect(Eof).unwrap();

    let messages = common::session_messages(home.path(), &session);
    let tool = |name: &str| {
        messages
            .iter()
            .find(|m| m["role"] == "tool" && m["tool_name"] == name)
            .and_then(|m| m["content"].as_str())
            .unwrap_or_else(|| panic!("no {name} result: {messages:#?}"))
            .to_owned()
    };
    let read = tool("read_file");
    assert!(read.contains("version before the crash"), "reused: {read}");
    assert_eq!(
        tool("terminal"),
        "[prometheus] outcome unknown: the process stopped while 'terminal' was running. Verify its effect before retrying."
    );
    let last = messages
        .iter()
        .rev()
        .find(|m| m["role"] == "assistant")
        .unwrap();
    assert_eq!(last["content"], "checked the deploy", "the loop went on");
    assert!(
        !work(home.path()).join("deployed").exists(),
        "terminal never re-ran"
    );
}

/// The daemon resumes a crashed turn when it loads the session
/// (`agent.resume_on_crash = "auto"`), before the next message runs.
#[cfg(feature = "gateway")]
#[test]
fn the_daemon_resumes_a_crashed_turn_when_the_session_loads() {
    let home = home();
    crash(home.path());
    let session = common::newest_session(home.path());
    common::append_config(home.path(), "\n[agent]\nresume_on_crash = \"auto\"\n");
    let _daemon = common::daemon::start(home.path(), &[]);
    let out = common::command(home.path())
        .current_dir(work(home.path()))
        .args(["--json", "-r", &session, "-z", "anything else?"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_str(&common::ok(&out)).unwrap();
    assert_eq!(report["answer"], "nothing else");

    let messages = common::session_messages(home.path(), &session);
    let texts: Vec<String> = messages
        .iter()
        .filter(|m| m["role"] != "user" || m["content"] == "anything else?")
        .map(|m| {
            format!(
                "{}: {}",
                m["role"].as_str().unwrap(),
                m["content"].as_str().unwrap_or("")
            )
        })
        .collect();
    let at = |needle: &str| {
        texts
            .iter()
            .position(|t| t.contains(needle))
            .unwrap_or_else(|| panic!("{needle}: {texts:#?}"))
    };
    assert!(
        at("outcome unknown") < at("checked the deploy"),
        "{texts:#?}"
    );
    assert!(
        at("checked the deploy") < at("anything else?"),
        "resumed first: {texts:#?}"
    );
}
