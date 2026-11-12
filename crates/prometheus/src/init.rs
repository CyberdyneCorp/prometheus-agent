//! `prometheus` as PID 1 of a container (spec: distribution-and-update,
//! Container filesystem, init and health). PID 1 inherits every orphaned
//! process of the container and receives `docker stop`'s SIGTERM, but the
//! kernel delivers no default action to it; a daemon that is PID 1 itself
//! would leave `<defunct>` entries and ignore SIGTERM. So when started as
//! PID 1, the binary becomes a minimal init instead: it runs the same
//! command line as a child, forwards SIGTERM, SIGINT, SIGHUP, SIGQUIT,
//! SIGUSR1 and SIGUSR2 to it, reaps every child that exits, and exits with
//! the child's status. A gateway daemon that exits 75 (`daemon restart`,
//! an update handover) is started again in place, so the container keeps
//! running across a restart.

/// Runs the init when this process is PID 1 (Linux); returns its exit
/// code, or `None` to run normally. `respawn` restarts a child that
/// exits 75 (the gateway daemon).
#[must_use]
pub fn run_if_pid1(respawn: bool) -> Option<u8> {
    #[cfg(target_os = "linux")]
    {
        if std::process::id() == 1 {
            return Some(linux::run(respawn));
        }
    }
    let _ = respawn;
    None
}

#[cfg(target_os = "linux")]
mod linux {
    use std::process::{Child, Command};

    use nix::errno::Errno;
    use nix::sys::signal::{SigSet, Signal, kill};
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    use nix::unistd::Pid;

    use pmt_core::supervisor::SUPERVISOR_ENV;

    /// The signals passed on to the child.
    const FORWARDED: [Signal; 6] = [
        Signal::SIGTERM,
        Signal::SIGINT,
        Signal::SIGHUP,
        Signal::SIGQUIT,
        Signal::SIGUSR1,
        Signal::SIGUSR2,
    ];

    /// Exit code of a daemon asking to be restarted.
    const RESTART: i32 = 75;

    pub(super) fn run(respawn: bool) -> u8 {
        let mut signals = SigSet::empty();
        signals.add(Signal::SIGCHLD);
        for signal in FORWARDED {
            signals.add(signal);
        }
        let mut init = match spawn_unblocked(&signals) {
            Ok(child) => Init {
                child,
                respawn,
                stopping: false,
            },
            Err(code) => return code,
        };
        // A child that exited before the signals were blocked sent its
        // SIGCHLD too early: look once before waiting.
        let mut reaped = init.child_exited(&signals);
        loop {
            if let Some(code) = reaped {
                return code;
            }
            reaped = match signals.wait() {
                Ok(Signal::SIGCHLD) => init.child_exited(&signals),
                Ok(signal) => {
                    init.forward(signal);
                    None
                }
                Err(_) => None,
            };
        }
    }

    /// The command this init runs.
    struct Init {
        child: Child,
        /// Start the command again after exit 75.
        respawn: bool,
        /// SIGTERM or SIGINT arrived: no respawn.
        stopping: bool,
    }

    impl Init {
        fn forward(&mut self, signal: Signal) {
            self.stopping |= matches!(signal, Signal::SIGTERM | Signal::SIGINT);
            let _ = kill(pid(&self.child), signal);
        }

        /// Reaps what exited; the exit code once the command is over (a
        /// restart exit starts it again instead).
        fn child_exited(&mut self, signals: &SigSet) -> Option<u8> {
            let code = reap(pid(&self.child))?;
            if !(self.respawn && code == RESTART && !self.stopping) {
                return Some(u8::try_from(code).unwrap_or(1));
            }
            match spawn_unblocked(signals) {
                Ok(next) => {
                    self.child = next;
                    self.child_exited(signals)
                }
                Err(code) => Some(code),
            }
        }
    }

    /// Starts the command with the signals unblocked (a child inherits the
    /// mask), then blocks them here to wait for them synchronously.
    fn spawn_unblocked(signals: &SigSet) -> Result<Child, u8> {
        let _ = signals.thread_unblock();
        let child = spawn();
        if let Err(e) = signals.thread_block() {
            eprintln!("prometheus init: cannot block signals: {e}");
            return Err(1);
        }
        child
    }

    fn spawn() -> Result<Child, u8> {
        let exe = std::env::current_exe().map_err(|e| {
            eprintln!("prometheus init: cannot find this binary: {e}");
            1u8
        })?;
        Command::new(exe)
            .args(std::env::args_os().skip(1))
            .env(SUPERVISOR_ENV, "init")
            .spawn()
            .map_err(|e| {
                eprintln!("prometheus init: cannot start the command: {e}");
                1
            })
    }

    fn pid(child: &Child) -> Pid {
        Pid::from_raw(i32::try_from(child.id()).unwrap_or(i32::MAX))
    }

    /// Reaps every exited process; the exit code of `main` when it is one
    /// of them (`128 + n` for a death by signal `n`).
    fn reap(main: Pid) -> Option<i32> {
        let mut main_code = None;
        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(pid, code)) if pid == main => main_code = Some(code),
                Ok(WaitStatus::Signaled(pid, signal, _)) if pid == main => {
                    main_code = Some(128 + signal as i32);
                }
                Ok(WaitStatus::StillAlive) | Err(Errno::ECHILD) => return main_code,
                Ok(_) | Err(Errno::EINTR) => {}
                Err(_) => return main_code,
            }
        }
    }
}
