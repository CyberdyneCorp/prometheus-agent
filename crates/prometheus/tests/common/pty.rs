//! The binary in a pseudo-terminal (expectrl), for line-interface and
//! TUI tests.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use expectrl::session::OsSession;
use expectrl::{Eof, Expect};

/// Global flags that keep a run on a terminal in the line interface
/// (`ui.interface` defaults to the TUI).
pub const LINE: [&str; 2] = ["--set", "ui.interface=line"];

/// The binary in a pseudo-terminal, run from the home directory, with
/// extra environment `env`, in the line interface.
pub fn spawn_with(home: &Path, args: &[&str], env: &[(&str, &str)]) -> OsSession {
    let mut cmd: Command = super::command(home);
    cmd.args(LINE)
        .args(args)
        .current_dir(home)
        .env("TERM", "xterm")
        .env_remove("NO_COLOR");
    for (key, value) in env {
        cmd.env(key, value);
    }
    let mut session = OsSession::spawn(cmd).unwrap();
    session.set_expect_timeout(Some(Duration::from_secs(30)));
    session
}

/// [`spawn_with`] without extra environment.
pub fn spawn(home: &Path, args: &[&str]) -> OsSession {
    spawn_with(home, args, &[])
}

/// Waits for the idle prompt.
pub fn prompt(p: &mut OsSession) {
    p.expect("› ").unwrap();
}

/// Sends `/exit` and waits for the process to end with status 0.
pub fn exit(mut p: OsSession) {
    p.send_line("/exit").unwrap();
    p.expect(Eof).unwrap();
    let status = p.get_process_mut().wait().unwrap();
    assert!(format!("{status:?}").ends_with(", 0)"), "{status:?}");
}

/// Writes an executable shell script and returns its path.
pub fn script(dir: &Path, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

/// A job-control shell in its own pseudo-terminal, clean environment.
pub fn shell(home: &Path) -> OsSession {
    let user_home = super::user_home(home);
    std::fs::create_dir_all(&user_home).unwrap();
    let mut cmd = Command::new("/usr/bin/env");
    cmd.args(["-i", "TERM=xterm", "PS1=$ "])
        .arg(format!(
            "PATH={}",
            std::env::var("PATH").unwrap_or_default()
        ))
        .arg(format!("HOME={}", user_home.display()))
        .args(["/bin/sh", "-i"])
        .current_dir(home);
    let mut session = OsSession::spawn(cmd).unwrap();
    session.set_expect_timeout(Some(Duration::from_secs(30)));
    session
}

/// Waits (at most 30 s) until `count` turns have started in the home's
/// default profile, reading the terminal meanwhile so the interface never
/// blocks on a full pseudo-terminal; what it printed stays for the next
/// `expect`.
pub fn wait_for_turns(p: &mut OsSession, home: &Path, count: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while super::turns_started(home) < count {
        assert!(
            std::time::Instant::now() < deadline,
            "{count} turn(s) did not start within 30 s"
        );
        let _ = p.check("\u{1}");
        std::thread::sleep(Duration::from_millis(20));
    }
}
