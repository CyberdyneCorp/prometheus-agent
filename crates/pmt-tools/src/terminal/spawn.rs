//! Starting confined shell processes and stopping them with everything
//! they started.
//!
//! On Unix each command leads its own process group, so a timeout, a
//! `kill` or the end of the session signals the whole group (SIGTERM, then
//! SIGKILL after `tools.kill_grace_secs`). On Windows the sandbox starts
//! children in a Job Object that is closed with its launcher, which kills
//! the tree.

use std::ffi::OsString;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use pmt_core::error::{Error, codes};
use pmt_sandbox::Sandbox;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::{Child, Command};

use super::env::ChildEnv;

/// Why a process could not be started.
#[derive(Debug)]
pub enum SpawnError {
    /// The sandbox refused (`sandbox unavailable: ...`): not retryable.
    Refused(Error),
    /// The execution environment failed (setup or spawn error): a
    /// degraded result, and the environment is recreated next time.
    Infrastructure(String),
}

/// A started process and its group.
#[derive(Debug)]
pub struct Spawned {
    /// The shell (or the sandbox launcher in front of it).
    pub child: Child,
    /// Its process group.
    pub group: Group,
}

/// The process group of a command (Unix), for signalling the whole tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Group(Option<i32>);

impl Group {
    /// Sends SIGTERM to every member.
    pub fn terminate(self) {
        self.signal(Signal::Term);
    }

    /// Sends SIGKILL to every member.
    pub fn kill(self) {
        self.signal(Signal::Kill);
    }

    #[cfg_attr(not(unix), allow(clippy::unused_self))]
    fn signal(self, signal: Signal) {
        #[cfg(unix)]
        if let Some(pgid) = self.0 {
            use nix::sys::signal::{Signal as Sig, killpg};
            let sig = match signal {
                Signal::Term => Sig::SIGTERM,
                Signal::Kill => Sig::SIGKILL,
            };
            // ESRCH (already gone) is fine.
            let _ = killpg(nix::unistd::Pid::from_raw(pgid), sig);
        }
        #[cfg(not(unix))]
        let _ = signal;
    }
}

#[derive(Debug, Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

/// Stops the group when dropped, so a cancelled or abandoned call never
/// leaves processes behind: SIGTERM at once (the spec's interrupt signal,
/// letting the command clean up), then SIGKILL after `grace` from a
/// short-lived thread, since a drop cannot wait. [`GroupGuard::disarm`]
/// once the group is known to be gone, so a later reuse of its id is never
/// signalled.
#[derive(Debug)]
pub struct GroupGuard {
    group: Option<Group>,
    grace: Duration,
}

impl GroupGuard {
    /// Guards `group`; `grace` is `tools.kill_grace_secs`.
    #[must_use]
    pub fn new(group: Group, grace: Duration) -> Self {
        Self {
            group: Some(group),
            grace,
        }
    }

    /// Stops guarding.
    pub fn disarm(&mut self) {
        self.group = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        let Some(group) = self.group else {
            return;
        };
        group.terminate();
        if group.0.is_some() {
            let grace = self.grace;
            std::thread::spawn(move || {
                std::thread::sleep(grace);
                group.kill();
            });
        }
    }
}

/// Starts `argv` confined by `sandbox` with exactly `env`, in `cwd`, with
/// stdout and stderr piped and stdin piped (`stdin`) or closed.
///
/// # Errors
///
/// [`SpawnError::Refused`] for a sandbox refusal,
/// [`SpawnError::Infrastructure`] otherwise.
///
/// # Panics
///
/// When `argv` is empty.
pub fn spawn(
    sandbox: &Sandbox,
    env: &ChildEnv,
    argv: &[OsString],
    cwd: &Path,
    stdin: bool,
) -> Result<Spawned, SpawnError> {
    let (program, rest) = argv.split_first().expect("argv has a program");
    // Checked here because a confining launcher (sandbox-exec) would start
    // fine and only fail later, as if the command itself had failed.
    if Path::new(program).is_absolute() && !Path::new(program).is_file() {
        return Err(SpawnError::Infrastructure(format!(
            "the shell {} is missing",
            Path::new(program).display()
        )));
    }
    let mut command = sandbox.command(program).map_err(|err| {
        if err.code() == &codes::SANDBOX_UNAVAILABLE {
            SpawnError::Refused(err)
        } else {
            SpawnError::Infrastructure(err.message().to_owned())
        }
    })?;
    command.args(rest).current_dir(cwd);
    env.apply(&mut command);
    command
        .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut command = Command::from(command);
    command.kill_on_drop(true);
    let child = command.spawn().map_err(|err| {
        SpawnError::Infrastructure(format!(
            "cannot start {}: {err}",
            Path::new(program).display()
        ))
    })?;
    let group = Group(if cfg!(unix) {
        child.id().and_then(|id| i32::try_from(id).ok())
    } else {
        None
    });
    Ok(Spawned { child, group })
}

/// Stops `child` and its group: SIGTERM, then SIGKILL after `grace`.
pub async fn stop(child: &mut Child, group: Group, grace: Duration) -> Option<ExitStatus> {
    group.terminate();
    if cfg!(not(unix)) {
        let _ = child.start_kill();
    }
    let status = if let Ok(status) = tokio::time::timeout(grace, child.wait()).await {
        status.ok()
    } else {
        group.kill();
        let _ = child.start_kill();
        child.wait().await.ok()
    };
    // Members that ignored SIGTERM or outlived the leader.
    group.kill();
    status
}

/// The exit code as a shell reports it (128 + signal for a signal death).
#[must_use]
pub fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return 128 + signal;
    }
    status.code().unwrap_or(-1)
}

/// Copies `reader` into `sink` until EOF.
pub async fn pump(mut reader: impl AsyncRead + Unpin, mut sink: impl FnMut(&[u8])) {
    let mut buf = vec![0u8; 16 * 1024];
    while let Ok(read) = reader.read(&mut buf).await {
        if read == 0 {
            break;
        }
        sink(&buf[..read]);
    }
}
