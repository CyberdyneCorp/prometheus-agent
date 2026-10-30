//! Helpers shared by the CLI tests: the built binary run against a
//! temporary home with a clean environment.

#![allow(dead_code, unreachable_pub)]

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

pub const BIN: &str = env!("CARGO_BIN_EXE_prometheus");

/// A temporary directory under the target dir (never the real home).
pub fn temp_home() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

/// The binary with no inherited Prometheus, editor, log or secret variables.
pub fn command(home: &Path) -> Command {
    let mut cmd = Command::new(BIN);
    for (var, _) in std::env::vars_os() {
        let name = var.to_string_lossy();
        if name.starts_with("PMT_") || name.starts_with("PROMETHEUS_") {
            cmd.env_remove(&var);
        }
    }
    for var in ["VISUAL", "EDITOR", "RUST_LOG"] {
        cmd.env_remove(var);
    }
    // Provider keys a developer machine may export would change routing.
    for var in pmt_providers::builtin::secret_names() {
        cmd.env_remove(var);
    }
    isolate_user_home(&mut cmd, home);
    cmd.arg("--home").arg(home);
    cmd
}

/// The user home every test process sees: a directory inside the test's
/// Prometheus home, so no test reads the developer's own agent CLI
/// logins (`~/.claude`, `~/.codex`) or other dotfiles.
pub fn user_home(home: &Path) -> std::path::PathBuf {
    home.join("user-home")
}

/// Points `HOME` (and `USERPROFILE`) of `cmd` at [`user_home`].
pub fn isolate_user_home(cmd: &mut Command, home: &Path) {
    let user = user_home(home);
    std::fs::create_dir_all(&user).unwrap();
    cmd.env("HOME", &user).env("USERPROFILE", &user);
}

/// Runs `args` with stdin closed.
pub fn run(home: &Path, args: &[&str]) -> Output {
    command(home)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// Runs `args` with `input` on stdin (a pipe, never a terminal).
pub fn run_with_stdin(home: &Path, args: &[&str], input: &str) -> Output {
    run_cmd_with_stdin(command(home).args(args), input)
}

/// Runs a prepared command with `input` on stdin.
pub fn run_cmd_with_stdin(cmd: &mut Command, input: &str) -> Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let written = child.stdin.take().unwrap().write_all(input.as_bytes());
    // A command that refuses its arguments exits before reading stdin; the
    // pipe is then closed, which is not a test failure.
    if let Err(err) = written {
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe, "{err}");
    }
    child.wait_with_output().unwrap()
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

/// Asserts success and returns stdout.
pub fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "exit {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        stdout(out),
        stderr(out)
    );
    stdout(out)
}

/// Every file under `dir`, recursively.
pub fn files_under(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found
}
