//! A TUI whose terminal goes away (spec: tui, Terminal state is always
//! restored: on `SIGHUP`). When the process holding a pseudo-terminal's
//! master dies, the TUI's controlling terminal hangs up: it stays readable
//! with nothing to read, and crossterm's read loop spins there without
//! honouring its poll timeout. The TUI must still restore and exit on the
//! hangup instead of waiting for that reader forever.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::daemon;
use common::tui::{opened, tui};
use expectrl::Expect as _;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

/// Where the holder finds the home.
const HOME_VAR: &str = "PMT_TEST_HANGUP_HOME";

/// The file the holder writes the TUI's pid to.
fn pid_file(home: &Path) -> PathBuf {
    home.join("tui.pid")
}

/// Whether `pid` still exists (not yet reaped).
fn alive(pid: Pid) -> bool {
    kill(pid, None).is_ok()
}

/// Run by [`a_hung_up_terminal_ends_the_tui`] in a process of its own: owns
/// the TUI's pseudo-terminal master until it is killed, which hangs the
/// terminal up as a closed terminal window or a dropped SSH session does.
#[test]
#[ignore = "run by a_hung_up_terminal_ends_the_tui"]
fn terminal_holder() {
    let home = PathBuf::from(std::env::var_os(HOME_VAR).expect("run by the hangup test"));
    let mut p = tui(&home, &[], &[]);
    opened(&mut p);
    let pid = p.get_process().pid().as_raw();
    std::fs::write(pid_file(&home), pid.to_string()).unwrap();
    // Keep reading so the TUI never blocks on a full terminal.
    loop {
        let _ = p.check("\u{1}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Task 23.2: the terminal hangs up (its master closes) and `SIGHUP`
/// arrives; the TUI exits within a few seconds.
#[test]
fn a_hung_up_terminal_ends_the_tui() {
    let home = common::mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let mut holder = Command::new(std::env::current_exe().unwrap())
        .args(["terminal_holder", "--exact", "--ignored", "--nocapture"])
        .env(HOME_VAR, home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let file = pid_file(home.path());
    let deadline = Instant::now() + Duration::from_secs(30);
    let pid = loop {
        if let Ok(text) = std::fs::read_to_string(&file)
            && let Ok(pid) = text.parse()
        {
            break Pid::from_raw(pid);
        }
        if Instant::now() > deadline {
            let _ = holder.kill();
            let _ = holder.wait();
            panic!("the TUI did not open within 30 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // Let the screen settle, then hang up: the master closes with the holder.
    std::thread::sleep(Duration::from_millis(300));
    holder.kill().unwrap();
    holder.wait().unwrap();
    // The kernel sends SIGHUP to the terminal's foreground group; send it
    // too, in case the TUI is not in it.
    std::thread::sleep(Duration::from_millis(300));
    let _ = kill(pid, Signal::SIGHUP);
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let stuck = alive(pid);
    if stuck {
        // Leave no orphan behind.
        let _ = kill(pid, Signal::SIGKILL);
    }
    assert!(!stuck, "the TUI was still running 10 s after the hangup");
}
