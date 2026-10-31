//! The pre-update backup (spec: distribution-and-update, Pre-update backup;
//! Schema migrations around updates), taken after the staged build passed
//! its self-test and before the swap:
//!
//! - `state` (default): a `pre-update` snapshot of every profile (config,
//!   grants, a consistent copy of `state.db`, ...), the newest
//!   `update.snapshot_keep` kept;
//! - `full`: the snapshots plus a whole-home archive (the `backup create`
//!   format, without runtimes and caches) at
//!   `updates/backups/pre-update-<from>-<to>.tar.zst`, the newest 2 kept;
//! - `off`: nothing, except the `pre-migration` snapshot of every profile
//!   whose store the new build will migrate, which a rollback needs.
//!
//! The space it needs is checked first, and any failure stops the update
//! before the swap with `update.backup_failed` (the CLI exits 73).

use std::path::{Path, PathBuf};

use pmt_backup::CreateOptions;
use pmt_backup::snapshot::{self, Reason};
use pmt_config::ProfileDir;
use pmt_config::daemon::PreUpdateBackup;
use pmt_core::error::{Error, codes};

use super::Install;
use crate::probe::BuildInfo;
use crate::receipts::{BackupRef, SnapshotRef};
use crate::version::ReleaseVersion;

/// How many full pre-update archives are kept.
pub const ARCHIVES_KEPT: usize = 2;
/// Head room left on the volume besides the backup itself.
const MARGIN: u64 = 16 * 1024 * 1024;

/// What the backup step wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Taken {
    /// Profile snapshots (pre-update, or pre-migration with `off`).
    pub snapshots: Vec<SnapshotRef>,
    /// Every snapshot and archive with its size, for the receipt.
    pub backups: Vec<BackupRef>,
}

/// One update's backup inputs.
pub(super) struct Step<'a> {
    pub install: &'a Install,
    pub mode: PreUpdateBackup,
    pub snapshot_keep: u32,
    pub staged: &'a BuildInfo,
    pub to: &'a ReleaseVersion,
}

/// Runs the backup step.
///
/// # Errors
///
/// `update.backup_failed` when space is short or anything cannot be
/// written.
pub(super) fn run(step: &Step<'_>) -> Result<Taken, Error> {
    let profiles = profiles(step.install)?;
    let snapshot_of: Vec<&ProfileDir> = match step.mode {
        PreUpdateBackup::State | PreUpdateBackup::Full => profiles.iter().collect(),
        PreUpdateBackup::Off => profiles
            .iter()
            .filter(|p| migrates(p, step.staged))
            .collect(),
    };
    let needed = snapshot_of.iter().map(|p| quick_size(p)).sum::<u64>()
        + if step.mode == PreUpdateBackup::Full {
            home_size(step.install.home.root())
        } else {
            0
        };
    check_space(needed, available_space(step.install.home.root()))?;
    let reason = if step.mode == PreUpdateBackup::Off {
        Reason::PreMigration
    } else {
        Reason::PreUpdate
    };
    let mut taken = Taken::default();
    for profile in snapshot_of {
        snapshot_profile(profile, reason, step, &mut taken).map_err(|e| failed(&e))?;
    }
    if step.mode == PreUpdateBackup::Full {
        let archive = full_archive(step).map_err(|e| failed(&e))?;
        taken.backups.push(archive);
    }
    Ok(taken)
}

/// Refuses a backup that does not fit (`available` is `None` when the
/// volume cannot be measured: the write itself then decides).
///
/// # Errors
///
/// `update.backup_failed`.
pub fn check_space(needed: u64, available: Option<u64>) -> Result<(), Error> {
    match available {
        Some(free) if free < needed.saturating_add(MARGIN) => Err(Error::new(
            &codes::UPDATE_BACKUP_FAILED,
            format!(
                "not enough space for the pre-update backup: it needs {needed} bytes, {free} are free"
            ),
        )
        .with_hint("free space on the home volume, or rerun with --no-backup")),
        _ => Ok(()),
    }
}

fn failed(err: &Error) -> Error {
    Error::new(
        &codes::UPDATE_BACKUP_FAILED,
        format!("cannot write the pre-update backup: {}", err.message()),
    )
    .with_hint("free space on the home volume, or rerun with --no-backup")
}

fn profiles(install: &Install) -> Result<Vec<ProfileDir>, Error> {
    install
        .home
        .profiles()?
        .iter()
        .map(|name| install.home.existing_profile(name))
        .collect()
}

/// Whether the staged build will migrate this profile's store.
fn migrates(profile: &ProfileDir, staged: &BuildInfo) -> bool {
    pmt_store::inspect::schema_version(profile)
        .ok()
        .flatten()
        .is_some_and(|schema| schema < staged.store_schema_version)
}

fn snapshot_profile(
    profile: &ProfileDir,
    reason: Reason,
    step: &Step<'_>,
    taken: &mut Taken,
) -> Result<(), Error> {
    let label = format!("pre-{}", step.to);
    let info = snapshot::create(profile, reason, Some(&label))?;
    if reason == Reason::PreUpdate {
        let keep = usize::try_from(step.snapshot_keep).unwrap_or(usize::MAX);
        snapshot::prune_reason(profile, Reason::PreUpdate, keep)?;
    }
    let name = profile.name().to_string();
    taken.backups.push(BackupRef {
        kind: "snapshot".to_owned(),
        path: profile.path().join("snapshots").join(&info.id),
        bytes: info.members.iter().map(|m| m.size).sum(),
    });
    taken.snapshots.push(SnapshotRef {
        profile: name,
        id: info.id,
        created_at: info.created_at,
    });
    Ok(())
}

fn full_archive(step: &Step<'_>) -> Result<BackupRef, Error> {
    let dir = backups_dir(step.install.home.root());
    pmt_core::private_fs::create_dir(&dir).map_err(|e| crate::install::io_error(&dir, &e))?;
    let path = dir.join(format!(
        "pre-update-{}-{}.tar.zst",
        step.install.version, step.to
    ));
    let created = pmt_backup::create(
        &step.install.home,
        &CreateOptions {
            output: Some(path.clone()),
            quick: false,
            label: None,
            // `backup create`'s own default-named archives are not ours to prune.
            keep: usize::MAX,
        },
    )?;
    prune_archives(&dir, &created.path);
    let bytes = std::fs::metadata(&created.path).map_or(created.bytes, |m| m.len());
    Ok(BackupRef {
        kind: "archive".to_owned(),
        path,
        bytes,
    })
}

/// Keeps the newest [`ARCHIVES_KEPT`] pre-update archives (`just_written`
/// always among them).
fn prune_archives(dir: &Path, just_written: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut archives: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("pre-update-") && name.ends_with(".tar.zst")
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .filter(|(_, path)| path != just_written)
        .collect();
    archives.sort();
    let excess = (archives.len() + 1).saturating_sub(ARCHIVES_KEPT);
    for (_, old) in archives.into_iter().take(excess) {
        let _ = std::fs::remove_file(old);
    }
}

/// `<home>/updates/backups`.
#[must_use]
pub fn backups_dir(home: &Path) -> PathBuf {
    home.join("updates").join("backups")
}

/// Bytes a quick snapshot of `profile` copies (its quick member set).
fn quick_size(profile: &ProfileDir) -> u64 {
    let (members, _) =
        pmt_backup::members::walk(profile.path(), &pmt_backup::members::profile_quick);
    members
        .iter()
        .filter_map(|m| std::fs::metadata(pmt_backup::members::on_disk(profile.path(), m)).ok())
        .map(|m| m.len())
        .sum()
}

/// Bytes a full archive of the home reads (an upper bound: it compresses).
fn home_size(home: &Path) -> u64 {
    let (members, _) = pmt_backup::members::walk(home, &|_| true);
    members
        .iter()
        .filter_map(|m| std::fs::metadata(pmt_backup::members::on_disk(home, m)).ok())
        .map(|m| m.len())
        .sum()
}

/// Free bytes on the volume holding `path` (the mount point that is its
/// longest prefix).
fn available_space(path: &Path) -> Option<u64> {
    let path = std::fs::canonicalize(path).ok()?;
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(sysinfo::Disk::available_space)
}

#[cfg(test)]
mod tests {
    use super::{MARGIN, check_space};

    #[test]
    fn a_volume_without_room_refuses_the_backup() {
        let err = check_space(1_000, Some(500)).unwrap_err();
        assert_eq!(err.code().code, "update.backup_failed");
        assert!(
            err.message().contains("needs 1000 bytes, 500 are free"),
            "{err}"
        );
        assert!(check_space(1_000, Some(1_000 + MARGIN)).is_ok());
        assert!(check_space(1_000, None).is_ok());
    }
}
