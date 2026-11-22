//! A test home that takes its daemon with it: whatever started a daemon
//! on the home (`daemon start`, a foreground `daemon run`, a command that
//! starts one on demand), the daemon is stopped and, failing that, killed
//! before the directory is removed. A daemon left running would outlive
//! the test process (it is detached) and keep serving a deleted home.

use std::ops::Deref;
use std::path::Path;
use std::time::{Duration, Instant};

use pmt_core::runtime::{Role, live_record};

/// How long a daemon gets to stop on request before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// A temporary home under the target dir; its daemon is stopped on drop.
pub struct TestHome(tempfile::TempDir);

impl TestHome {
    /// A new empty home.
    pub fn new() -> Self {
        Self(tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap())
    }
}

impl Deref for TestHome {
    type Target = tempfile::TempDir;

    fn deref(&self) -> &tempfile::TempDir {
        &self.0
    }
}

impl AsRef<Path> for TestHome {
    fn as_ref(&self) -> &Path {
        self.0.path()
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        reap(self.0.path());
    }
}

/// The pid of `home`'s running daemon.
pub fn daemon_pid(home: &Path) -> Option<u32> {
    live_record(home, Role::Daemon)
        .ok()
        .flatten()
        .map(|r| r.pid)
}

/// Stops `home`'s daemon (asking first, so it stops its own children),
/// kills it when it does not stop in time, and waits until it is gone.
pub fn reap(home: &Path) {
    let Some(pid) = daemon_pid(home) else {
        return;
    };
    let _ = super::command(home)
        .args(["daemon", "stop"])
        .stdin(std::process::Stdio::null())
        .output();
    if gone(home, pid, STOP_GRACE) {
        return;
    }
    kill(pid);
    gone(home, pid, STOP_GRACE);
}

/// Waits up to `limit` for `home`'s daemon `pid` to be gone.
fn gone(home: &Path, pid: u32, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while daemon_pid(home) == Some(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

fn kill(pid: u32) {
    let pid = pid.to_string();
    let mut command = if cfg!(windows) {
        let mut command = std::process::Command::new("taskkill");
        command.args(["/F", "/T", "/PID", &pid]);
        command
    } else {
        let mut command = std::process::Command::new("kill");
        command.args(["-KILL", &pid]);
        command
    };
    let _ = command.output();
}
