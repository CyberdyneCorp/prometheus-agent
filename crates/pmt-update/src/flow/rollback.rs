//! `prometheus update --rollback [--restore-snapshots]` (spec:
//! distribution-and-update, Rollback; Schema migrations around updates).
//!
//! The previous binary reports the newest store schema it reads
//! (`version --json`). A profile whose `state.db` is newer than that blocks
//! the rollback unless `--restore-snapshots` puts back the pre-update
//! snapshots the last update took, after the user confirms losing the
//! sessions created since.

use pmt_backup::snapshot;
use pmt_config::ProfileName;
use pmt_core::error::{Error, codes};

use super::check::begin;
use super::{Install, finish, refuse, self_updating};
use crate::lock::UpdateLock;
use crate::probe::{self, BuildInfo};
use crate::receipts::{self, Kind, Outcome, ReceiptWriter, SnapshotRef};
use crate::swap;
use crate::version::ReleaseVersion;

/// Options of `--rollback`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RollbackOptions {
    /// `--restore-snapshots`.
    pub restore_snapshots: bool,
}

/// Asks the user to confirm restoring snapshots; the argument is the
/// question. `None` when no answer can be obtained (non-interactive
/// without `--yes`: exit 4).
pub type Confirm<'a> = &'a dyn Fn(&str) -> Option<bool>;

/// What `--rollback` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RolledBack {
    /// The previous binary is back.
    Done {
        /// The restored version.
        version: String,
        /// Snapshots restored (with `--restore-snapshots`).
        restored: Vec<SnapshotRef>,
    },
    /// The user declined.
    Declined,
    /// Confirmation was needed but could not be asked (exit 4).
    NeedsConfirmation,
}

/// Runs `--rollback`.
///
/// # Errors
///
/// `update.owned_elsewhere` (69), `update.locked` (75),
/// `update.rollback_refused` (1) with the spec message, `update.failed`.
pub fn rollback(
    install: &Install,
    options: RollbackOptions,
    confirm: Confirm<'_>,
) -> Result<RolledBack, Error> {
    let receipt = self_updating(install, Kind::Rollback)?;
    let channel = receipt.channel;
    let _lock = UpdateLock::acquire(install.home.root(), &install.binary).inspect_err(|err| {
        let _ = refuse(install, Kind::Rollback, channel.as_str(), err);
    })?;
    let mut writer = begin(install, Kind::Rollback, channel)?;
    let result = run(install, &mut writer, options, confirm);
    let outcome = match &result {
        Ok(RolledBack::Done { .. }) => Outcome::RolledBack,
        Ok(_) => Outcome::Refused,
        Err(_) => Outcome::Failed,
    };
    finish(writer, result, outcome)
}

fn run(
    install: &Install,
    writer: &mut ReceiptWriter,
    options: RollbackOptions,
    confirm: Confirm<'_>,
) -> Result<RolledBack, Error> {
    let prev_path = swap::prev_path(&install.binary);
    if !prev_path.is_file() {
        return Err(Error::new(
            &codes::UPDATE_ROLLBACK_REFUSED,
            format!(
                "cannot roll back: there is no previous binary ({})",
                prev_path.display()
            ),
        ));
    }
    let prev = probe::version(&prev_path)?;
    let prev_version = display_version(&prev);
    writer.update(|r| r.to = Some(prev_version.clone()))?;
    let restored = if options.restore_snapshots {
        match restore_snapshots(install, confirm)? {
            Ok(restored) => restored,
            Err(not_confirmed) => return Ok(not_confirmed),
        }
    } else {
        refuse_newer_stores(install, &prev, &prev_version)?;
        Vec::new()
    };
    writer.stage("check")?;
    swap::restore_prev(&install.binary)?;
    writer.stage("swap")?;
    if let Some(mut receipt) = install.receipt()? {
        receipt.version.clone_from(&prev_version);
        receipt.write(install.home.root())?;
    }
    Ok(RolledBack::Done {
        version: prev_version,
        restored,
    })
}

/// Refuses when any profile's `state.db` is newer than `prev` reads.
fn refuse_newer_stores(
    install: &Install,
    prev: &BuildInfo,
    prev_version: &str,
) -> Result<(), Error> {
    for name in install.home.profiles()? {
        let profile = install.home.existing_profile(&name)?;
        let Some(schema) = pmt_store::inspect::schema_version(&profile)? else {
            continue;
        };
        if schema > prev.store_schema_version {
            return Err(Error::new(
                &codes::UPDATE_ROLLBACK_REFUSED,
                format!(
                    "cannot roll back: profile {name} state.db schema {schema} is newer than {prev_version} supports (max {}); rerun with --restore-snapshots to restore the pre-update snapshot",
                    prev.store_schema_version
                ),
            ));
        }
    }
    Ok(())
}

/// Restores the last update's snapshots after confirmation; the inner
/// `Err` is [`RolledBack::Declined`] or [`RolledBack::NeedsConfirmation`].
fn restore_snapshots(
    install: &Install,
    confirm: Confirm<'_>,
) -> Result<Result<Vec<SnapshotRef>, RolledBack>, Error> {
    let snapshots = receipts::last_successful_update(install.home.root())?
        .map(|r| r.snapshots)
        .unwrap_or_default();
    if snapshots.is_empty() {
        return Err(Error::new(
            &codes::UPDATE_ROLLBACK_REFUSED,
            "cannot roll back: the last update recorded no pre-update snapshots to restore",
        ));
    }
    let lost = sessions_since(install, &snapshots)?;
    let question = format!(
        "restoring the pre-update snapshots of {} profile(s) loses {lost} session(s) created since the update; continue?",
        snapshots.len()
    );
    match confirm(&question) {
        Some(true) => {}
        Some(false) => return Ok(Err(RolledBack::Declined)),
        None => return Ok(Err(RolledBack::NeedsConfirmation)),
    }
    for taken in &snapshots {
        let profile = install
            .home
            .existing_profile(&profile_name(&taken.profile)?)?;
        snapshot::restore(&profile, &taken.id)?;
    }
    Ok(Ok(snapshots))
}

fn sessions_since(install: &Install, snapshots: &[SnapshotRef]) -> Result<u64, Error> {
    let mut lost = 0;
    for taken in snapshots {
        let profile = install
            .home
            .existing_profile(&profile_name(&taken.profile)?)?;
        let since = taken
            .created_at
            .parse::<jiff::Timestamp>()
            .map_or(0, jiff::Timestamp::as_millisecond);
        lost += pmt_store::inspect::sessions_created_since(&profile, since)?;
    }
    Ok(lost)
}

fn profile_name(name: &str) -> Result<ProfileName, Error> {
    ProfileName::parse(name)
}

/// The previous build's version as a release tag.
fn display_version(prev: &BuildInfo) -> String {
    ReleaseVersion::parse(&prev.version).map_or_else(|_| prev.version.clone(), |v| v.to_string())
}
