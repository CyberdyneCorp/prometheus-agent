//! Process-tree control: the sidecar leads its own process group (Unix),
//! so terminating the group reaches every child the plugin started.

use std::process::{Command, ExitStatus};

/// Makes the spawned process lead a new process group.
pub(super) fn new_group(command: &mut Command) {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    #[cfg(not(unix))]
    let _ = command;
}

/// Signals the group `pid` leads: SIGTERM, or SIGKILL when `force`.
/// Elsewhere the caller kills the process itself (Windows sidecars run in
/// the sandbox's job object, which ends with them).
pub fn terminate_tree(pid: u32, force: bool) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        if let Ok(raw) = i32::try_from(pid) {
            let signal = if force {
                Signal::SIGKILL
            } else {
                Signal::SIGTERM
            };
            // ESRCH (already gone) is fine.
            let _ = killpg(Pid::from_raw(raw), signal);
        }
    }
    #[cfg(not(unix))]
    let _ = (pid, force);
}

/// `exit code 3`, `signal 11 (SIGSEGV)`.
#[must_use]
pub fn exit_text(status: &ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            let name = nix::sys::signal::Signal::try_from(signal)
                .map_or_else(|_| String::new(), |s| format!(" ({})", s.as_str()));
            return format!("signal {signal}{name}");
        }
    }
    status
        .code()
        .map_or_else(|| status.to_string(), |code| format!("exit code {code}"))
}
