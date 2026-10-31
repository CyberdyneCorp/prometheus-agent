//! The `prometheus update` operations: status, check, update, rollback and
//! set-channel (spec: distribution-and-update). Each runs the same
//! preamble: running receipts of dead processes become `interrupted`, and
//! an install owned elsewhere is refused (exit 69, `refused` receipt)
//! before any network request or mutation.

mod apply;
mod check;
mod rollback;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pmt_config::Home;
use pmt_config::daemon::UpdateConfig;
use pmt_core::error::{Error, codes};

pub use apply::{UpdateOptions, Updated, update};
pub use check::{Checked, check, set_channel};
pub use rollback::{Confirm, RollbackOptions, RolledBack, rollback};

use crate::fetch::Releases;
use crate::install::{InstallReceipt, Owner};
use crate::receipts::{self, Kind, Outcome, ReceiptWriter};
use crate::version::{Channel, ReleaseVersion};

/// The running install.
#[derive(Debug)]
pub struct Install {
    /// The running executable.
    pub binary: PathBuf,
    /// The home whose `updates/` holds the lock and receipts.
    pub home: Home,
    /// The running build's version.
    pub version: ReleaseVersion,
    /// The running build's target triple.
    pub target: String,
}

impl Install {
    /// The install receipt; `None` for a source build.
    ///
    /// # Errors
    ///
    /// `update.failed` for an unreadable receipt.
    pub fn receipt(&self) -> Result<Option<InstallReceipt>, Error> {
        InstallReceipt::read(&self.binary, self.home.root())
    }

    fn root(&self) -> &Path {
        self.home.root()
    }
}

/// What an update operation needs besides the install.
pub struct Session<'a> {
    /// The install being updated.
    pub install: &'a Install,
    /// `daemon.toml [update]`.
    pub config: &'a UpdateConfig,
    /// The release origin client.
    pub releases: &'a Releases,
    /// Progress lines (stderr in the CLI).
    pub progress: &'a dyn Fn(&str),
}

/// `prometheus update --status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// Who updates this install.
    pub owner: Owner,
    /// The persisted channel (`stable` without a receipt).
    pub channel: Channel,
    /// The running version.
    pub version: ReleaseVersion,
    /// The running target.
    pub target: String,
    /// The running binary.
    pub binary: PathBuf,
    /// The newest update receipt.
    pub last: Option<receipts::UpdateReceipt>,
}

impl Status {
    /// The text form: one `key: value` per line, then the update hint.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!(
            "owner: {}\nchannel: {}\nversion: {}\ntarget: {}\nbinary: {}\n",
            self.owner.as_str(),
            self.channel,
            self.version,
            self.target,
            self.binary.display()
        );
        if let Some(last) = &self.last {
            let _ = writeln!(
                out,
                "last update: {} ({})",
                last.outcome.as_str(),
                last.started_at
            );
        }
        if self.owner == Owner::Dev {
            out.push_str("this build does not self-update\n");
        }
        let _ = writeln!(out, "update with: {}", self.owner.update_command());
        out
    }
}

/// `prometheus update --status`: never touches the network.
///
/// # Errors
///
/// `update.failed` for an unreadable receipt.
pub fn status(install: &Install) -> Result<Status, Error> {
    let receipt = install.receipt()?;
    Ok(Status {
        owner: receipt.as_ref().map_or(Owner::Dev, |r| r.owner),
        channel: receipt.as_ref().map_or(Channel::Stable, |r| r.channel),
        version: install.version,
        target: install.target.clone(),
        binary: install.binary.clone(),
        last: receipts::latest(install.root()),
    })
}

/// The preamble of every operation: marks dead `running` receipts
/// `interrupted`, then returns the receipt of a self-updating install.
///
/// # Errors
///
/// `update.owned_elsewhere` (exit 69) after writing a `refused` receipt.
pub(crate) fn self_updating(install: &Install, kind: Kind) -> Result<InstallReceipt, Error> {
    receipts::mark_interrupted(install.root())?;
    let receipt = install.receipt()?;
    let owner = receipt.as_ref().map_or(Owner::Dev, |r| r.owner);
    match receipt {
        Some(receipt) if owner.self_updates() => Ok(receipt),
        _ => {
            let err = owned_elsewhere(owner);
            refuse(install, kind, "stable", &err)?;
            Err(err)
        }
    }
}

fn owned_elsewhere(owner: Owner) -> Error {
    let what = if owner == Owner::Dev {
        "this build does not self-update"
    } else {
        "this install is updated by its package manager"
    };
    Error::new(
        &codes::UPDATE_OWNED_ELSEWHERE,
        format!("owner: {}: {what}", owner.as_str()),
    )
    .with_hint(format!("update with: {}", owner.update_command()))
}

/// Writes a `refused` receipt for `err`.
pub(crate) fn refuse(
    install: &Install,
    kind: Kind,
    channel: &str,
    err: &Error,
) -> Result<(), Error> {
    let writer = ReceiptWriter::begin(
        install.root(),
        kind,
        &install.binary,
        &install.version.to_string(),
        channel,
    )?;
    writer.finish(Outcome::Refused, Some(err)).map(drop)
}

/// Ends `writer` with the outcome `result` implies.
pub(crate) fn finish<T>(
    writer: ReceiptWriter,
    result: Result<T, Error>,
    ok: Outcome,
) -> Result<T, Error> {
    match result {
        Ok(value) => {
            writer.finish(ok, None)?;
            Ok(value)
        }
        Err(err) => {
            writer.finish(Outcome::Failed, Some(&err))?;
            Err(err)
        }
    }
}
