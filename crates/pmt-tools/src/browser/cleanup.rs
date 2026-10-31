//! Killing a local browser with its helpers and removing its directories
//! (spec: execution-environments, Browser supervision, dialogs and cleanup).
//!
//! Chromium is a process tree: on Linux the zygote, renderers, GPU and
//! crashpad helpers keep writing to the profile directory for a moment
//! after the main process dies. Each local browser is therefore launched
//! as the leader of its own process group, killed as a group, and its
//! directories are removed with retries until they are really gone.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

/// How long removal keeps retrying while dying helpers still write.
const REMOVE_WAIT: Duration = Duration::from_secs(5);

/// Pause between removal attempts.
const REMOVE_PAUSE: Duration = Duration::from_millis(50);

/// Makes the browser spawned by `command` lead a new process group, so
/// [`kill_group`] reaches every helper it forks.
pub fn new_group(command: &mut std::process::Command) {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    #[cfg(not(unix))]
    let _ = command;
}

/// Sends SIGKILL to the process group `pid` leads (the browser and its
/// helpers). The caller has checked that `pid` is the browser it launched.
/// A browser that does not lead a group (recorded before groups were
/// used) gets the signal alone. Elsewhere this is a no-op: the caller
/// kills the browser process itself.
pub fn kill_group(pid: u32) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, kill, killpg};
        use nix::unistd::{Pid, getpgid};
        let Ok(raw) = i32::try_from(pid) else {
            return;
        };
        let pid = Pid::from_raw(raw);
        // ESRCH (already gone) is fine.
        if getpgid(Some(pid)) == Ok(pid) {
            let _ = killpg(pid, Signal::SIGKILL);
        } else {
            let _ = kill(pid, Signal::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Removes `dir` and everything below it, retrying while files reappear,
/// for at most [`REMOVE_WAIT`].
pub fn remove_dir(dir: &Path) {
    let deadline = Instant::now() + REMOVE_WAIT;
    while !try_remove(dir) && Instant::now() < deadline {
        std::thread::sleep(REMOVE_PAUSE);
    }
}

/// [`remove_dir`] for async callers.
pub async fn remove_dir_async(dir: &Path) {
    let deadline = Instant::now() + REMOVE_WAIT;
    while !try_remove(dir) && Instant::now() < deadline {
        tokio::time::sleep(REMOVE_PAUSE).await;
    }
}

/// One removal attempt; whether `dir` is gone.
fn try_remove(dir: &Path) -> bool {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => !dir.exists(),
        Err(err) => err.kind() == io::ErrorKind::NotFound,
    }
}
