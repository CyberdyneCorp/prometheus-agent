//! Running daemon handover (spec: distribution-and-update, Running daemon
//! handover): after the swap, a gateway daemon of the same binary drains
//! and restarts on the new build. The daemon is reached through the
//! client protocol, which this crate does not speak, so the binary
//! supplies a [`Daemon`] and the update records what it did: stage
//! `restart`, or follow-up [`RESTART_PENDING`] with the command to run.

use pmt_core::error::Error;

use crate::version::ReleaseVersion;

/// The follow-up an update leaves when the daemon still runs the old
/// build (`--no-restart`, or the drain timed out).
pub const RESTART_PENDING: &str = "daemon_restart_pending";

/// The running daemon, as the update sees it.
pub trait Daemon: std::fmt::Debug {
    /// Before the swap. An error stops the update with nothing changed:
    /// a busy daemon whose restart needs a confirmation that cannot be
    /// asked (`update.confirmation_required`, exit 4).
    ///
    /// # Errors
    ///
    /// As above.
    fn prepare(&self) -> Result<(), Error>;

    /// After the swap: drain and restart a daemon of this binary.
    fn hand_over(&self, to: &ReleaseVersion) -> HandedOver;
}

/// What happened to the running daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandedOver {
    /// No daemon of this binary was running.
    NotRunning,
    /// It restarted on this version.
    Restarted(String),
    /// It still runs the old build: why, and the command that restarts it.
    Pending {
        /// `--no-restart`, or the drain timeout.
        reason: String,
        /// What the user runs once its turns are done.
        command: String,
    },
}
