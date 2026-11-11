//! The running daemon's side of an update (spec: distribution-and-update,
//! Running daemon handover): after the swap, a gateway daemon started from
//! the replaced binary finishes its turns (up to `update.drain_timeout_secs`)
//! and restarts on the new build, under its supervisor when it has one
//! (container init, systemd, launchd) or started again by the update.
//! Turns still running at the timeout are never cut short: the daemon
//! keeps the old build and the update reports `daemon_restart_pending`.
//! Restarting a busy daemon needs a confirmation: asked on a terminal,
//! given by `--yes`, else the update stops before the swap (exit 4).

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pmt_core::error::{Error, codes};
use pmt_rpc::control::{ControlRequest, DaemonStatus};
use pmt_rpc::discovery::{DaemonRecord, Discovery, discover};
use pmt_update::ReleaseVersion;
use pmt_update::flow::{Daemon, HandedOver};

/// How long the daemon may take to stop once idle, and a new one to start.
const SWITCH: Duration = Duration::from_secs(30);

/// The command that restarts the daemon by hand.
const RESTART_COMMAND: &str = "prometheus daemon restart";

/// The daemon of a home, as an update hands it over.
#[derive(Debug)]
pub(super) struct RunningDaemon {
    /// The home whose daemon it is.
    pub home: PathBuf,
    /// The binary being replaced.
    pub binary: PathBuf,
    /// `--no-restart`.
    pub no_restart: bool,
    /// `--yes`: restart a busy daemon without asking.
    pub yes: bool,
    /// `update.drain_timeout_secs`.
    pub drain: Duration,
    /// Asks a yes/no question on the terminal (`None`: no terminal).
    pub ask: fn(&str) -> Option<bool>,
    /// The user declined restarting a busy daemon.
    declined: Cell<bool>,
}

impl RunningDaemon {
    pub(super) fn new(
        home: &Path,
        binary: &Path,
        no_restart: bool,
        yes: bool,
        drain: Duration,
    ) -> Self {
        Self {
            home: home.to_owned(),
            binary: binary.to_owned(),
            no_restart,
            yes,
            drain,
            ask: ask_on_terminal,
            declined: Cell::new(false),
        }
    }

    /// Whether a daemon of the binary being replaced runs.
    pub(super) fn is_running(&self) -> bool {
        self.find().is_some()
    }

    /// The daemon of this home started from the binary being replaced,
    /// with its status.
    fn find(&self) -> Option<(DaemonRecord, DaemonStatus)> {
        let Ok(Discovery::Running(record)) = discover(&self.home) else {
            return None;
        };
        let status = status_of(&record)?;
        (status.binary.as_deref() == Some(self.binary.as_path())).then_some((record, status))
    }

    /// Waits until no turn runs; `false` when some still run at the timeout.
    fn drained(&self, record: &DaemonRecord) -> (bool, usize) {
        let deadline = Instant::now() + self.drain;
        loop {
            let running = status_of(record).map_or(0, |s| s.active_turns);
            if running == 0 {
                return (true, 0);
            }
            if Instant::now() >= deadline {
                return (false, running);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn restart(&self, record: &DaemonRecord, supervised: bool) -> Result<String, String> {
        control(record, &ControlRequest::Stop { restart: true })?;
        let old = record.endpoint.instance_id.clone();
        if !supervised {
            wait_for(SWITCH, || !matches!(discover(&self.home), Ok(Discovery::Running(r)) if r.endpoint.instance_id == old))
                .then_some(())
                .ok_or("the daemon did not stop")?;
            let started = std::process::Command::new(&self.binary)
                .arg("--home")
                .arg(&self.home)
                .args(["daemon", "start"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .status()
                .map_err(|e| format!("cannot start the new daemon: {e}"))?;
            if !started.success() {
                return Err(format!("the new daemon did not start ({started})"));
            }
        }
        let mut version = None;
        wait_for(SWITCH, || match discover(&self.home) {
            Ok(Discovery::Running(new)) if new.endpoint.instance_id != old => {
                version = status_of(&new).map(|s| s.version);
                version.is_some()
            }
            _ => false,
        });
        version.ok_or_else(|| "the new daemon did not come up".to_owned())
    }
}

impl Daemon for RunningDaemon {
    fn prepare(&self) -> Result<(), Error> {
        let Some((record, status)) = self.find() else {
            return Ok(());
        };
        if self.no_restart || self.yes || status.active_turns == 0 {
            return Ok(());
        }
        let question = format!(
            "the daemon (pid {}) has {} running turn(s); restart it on the new build once they finish (up to {} s)?",
            record.process.pid,
            status.active_turns,
            self.drain.as_secs()
        );
        match (self.ask)(&question) {
            Some(true) => Ok(()),
            Some(false) => {
                self.declined.set(true);
                Ok(())
            }
            None => Err(Error::new(&codes::UPDATE_CONFIRMATION_REQUIRED, question).with_hint(
                "rerun with --yes to restart it once its turns finish, or --no-restart to keep it on the old build",
            )),
        }
    }

    fn hand_over(&self, _to: &ReleaseVersion) -> HandedOver {
        let Some((record, status)) = self.find() else {
            return HandedOver::NotRunning;
        };
        if self.no_restart {
            return pending("--no-restart".to_owned());
        }
        if self.declined.get() {
            return pending("restart declined".to_owned());
        }
        let (drained, running) = self.drained(&record);
        if !drained {
            return pending(format!(
                "{running} turn(s) still running after {} s",
                self.drain.as_secs()
            ));
        }
        match self.restart(&record, status.supervised) {
            Ok(version) => HandedOver::Restarted(version),
            Err(why) => pending(why),
        }
    }
}

/// The daemon keeps the old build: why, and how to restart it later.
fn pending(reason: String) -> HandedOver {
    HandedOver::Pending {
        reason,
        command: RESTART_COMMAND.to_owned(),
    }
}

fn ask_on_terminal(question: &str) -> Option<bool> {
    crate::prompt::stdin_is_tty()
        .then(|| crate::prompt::confirm(question).ok())
        .flatten()
}

fn status_of(record: &DaemonRecord) -> Option<DaemonStatus> {
    control(record, &ControlRequest::Status).ok()?.status
}

fn control(
    record: &DaemonRecord,
    request: &ControlRequest,
) -> Result<pmt_rpc::control::ControlResponse, String> {
    let address = record.endpoint.control.clone();
    let request = request.clone();
    // The update runs inside a runtime already: the call gets a thread
    // (and a runtime) of its own.
    std::thread::spawn(move || {
        crate::model_cmd::block_on(async move { pmt_rpc::control::call(&address, &request).await })
    })
    .join()
    .map_err(|_| "the control call panicked".to_owned())?
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

fn wait_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    done()
}
