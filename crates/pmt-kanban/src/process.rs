//! Worker processes are identified by pid plus OS start time (spec:
//! kanban, Worker liveness and heartbeats): a pid whose start time
//! changed belongs to another process and is never signalled.

use pmt_core::runtime::process_start_time;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System};

/// Whether `pid` still runs and started at `start` (Unix s).
#[must_use]
pub fn alive(pid: u32, start: u64) -> bool {
    process_start_time(pid) == Some(start)
}

/// What a termination did, as recorded in events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminated {
    /// The signal was delivered.
    Signalled,
    /// The process was already gone.
    Gone,
    /// The pid belongs to another process now: nothing was signalled.
    PidReused,
    /// The OS refused the signal.
    Failed,
}

impl Terminated {
    /// The recorded spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Signalled => "signalled",
            Self::Gone => "gone",
            Self::PidReused => "pid_reused",
            Self::Failed => "failed",
        }
    }
}

/// Terminates the worker `pid` started at `start`: SIGTERM where the OS
/// has signals, else a kill.
#[must_use]
pub fn terminate(pid: u32, start: u64) -> Terminated {
    let id = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[id]),
        true,
        ProcessRefreshKind::nothing(),
    );
    let Some(process) = system.process(id) else {
        return Terminated::Gone;
    };
    if process.start_time() != start {
        return Terminated::PidReused;
    }
    let sent = process
        .kill_with(Signal::Term)
        .unwrap_or_else(|| process.kill());
    if sent {
        Terminated::Signalled
    } else {
        Terminated::Failed
    }
}
