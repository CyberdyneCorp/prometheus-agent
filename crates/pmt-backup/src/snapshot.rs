//! Per-profile state snapshots (spec: configuration-and-profiles, State
//! snapshots): the critical state of one profile (the `--quick` member
//! set) under `<profile>/snapshots/snap_<ULID>/`, with a hashed manifest.
//!
//! Snapshots take a [`ProfileDir`], not a loaded scope, so a snapshot can
//! still restore a profile whose `config.toml` no longer loads.

use std::path::{Path, PathBuf};

use pmt_config::ProfileDir;
use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use pmt_core::runtime::Role;
use serde::{Deserialize, Serialize};

use crate::fsutil::{self, io_error};
use crate::manifest::Member;
use crate::{apply, members, sqlite};

const INFO_FILE: &str = "snapshot.json";
const FILES_DIR: &str = "files";

/// Why a snapshot was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    /// `prometheus snapshots create`, `/snapshot create`.
    Manual,
    /// Before `prometheus update` replaced the binary.
    PreUpdate,
    /// Before a snapshot or backup restore replaced the state.
    PreRestore,
    /// Before a store schema migration.
    PreMigration,
}

impl Reason {
    /// The reason as listed (`manual`, `pre-update`, ...).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::PreUpdate => "pre-update",
            Self::PreRestore => "pre-restore",
            Self::PreMigration => "pre-migration",
        }
    }

    /// Whether `snapshots.keep` bounds this reason. Update and migration
    /// snapshots follow the update retention instead.
    #[must_use]
    pub fn kept_by_snapshots_keep(self) -> bool {
        matches!(self, Self::Manual | Self::PreRestore)
    }
}

/// A snapshot's metadata (`snapshot.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    /// `snap_<ULID>`.
    pub id: String,
    /// Optional label.
    pub label: Option<String>,
    /// Why it was taken.
    pub reason: Reason,
    /// RFC 3339 creation time (UTC).
    pub created_at: String,
    /// The files, relative to the profile directory.
    pub members: Vec<Member>,
}

/// Snapshots `profile` now.
///
/// # Errors
///
/// `backup.io`.
///
/// # Panics
///
/// Never: the metadata always serializes.
pub fn create(
    profile: &ProfileDir,
    reason: Reason,
    label: Option<&str>,
) -> Result<SnapshotInfo, Error> {
    let id = pmt_core::id::SnapshotId::new().to_string();
    let root = snapshots_dir(profile);
    let partial = root.join(format!(".{id}.partial"));
    let written = write(profile.path(), &partial).and_then(|members| {
        let info = SnapshotInfo {
            id: id.clone(),
            label: label.map(str::to_owned),
            reason,
            created_at: jiff::Timestamp::now().to_string(),
            members,
        };
        let json = serde_json::to_vec_pretty(&info).expect("snapshot info serializes");
        let info_path = partial.join(INFO_FILE);
        private_fs::write_atomic(&info_path, &json).map_err(|e| io_error(&info_path, &e))?;
        let target = root.join(&id);
        std::fs::rename(&partial, &target).map_err(|e| io_error(&target, &e))?;
        Ok(info)
    });
    if written.is_err() {
        let _ = std::fs::remove_dir_all(&partial);
    }
    written
}

fn write(profile: &Path, partial: &Path) -> Result<Vec<Member>, Error> {
    let files = partial.join(FILES_DIR);
    private_fs::create_dir(&files).map_err(|e| io_error(&files, &e))?;
    let (selected, _) = members::walk(profile, &members::profile_quick);
    let mut written = Vec::new();
    for relative in selected {
        let source = members::on_disk(profile, &relative);
        let target = members::on_disk(&files, &relative);
        if let Some(parent) = target.parent() {
            private_fs::create_dir(parent).map_err(|e| io_error(parent, &e))?;
        }
        if members::is_database(&relative) {
            sqlite::copy(&source, &target)?;
        } else {
            private_fs::copy_file(&source, &target).map_err(|e| io_error(&source, &e))?;
        }
        let (size, sha256) = fsutil::hash_file(&target)?;
        written.push(Member {
            path: relative,
            size,
            sha256,
        });
    }
    Ok(written)
}

/// Every snapshot of `profile`, newest first.
///
/// # Errors
///
/// `backup.io`.
pub fn list(profile: &ProfileDir) -> Result<Vec<SnapshotInfo>, Error> {
    let root = snapshots_dir(profile);
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_error(&root, &e)),
    };
    let mut found: Vec<SnapshotInfo> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("snap_"))
        })
        .filter_map(|e| read_info(&e.path()).ok())
        .collect();
    // ULIDs sort by creation time.
    found.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(found)
}

fn read_info(dir: &Path) -> Result<SnapshotInfo, Error> {
    let path = dir.join(INFO_FILE);
    let bytes = std::fs::read(&path).map_err(|e| io_error(&path, &e))?;
    serde_json::from_slice(&bytes).map_err(|e| corrupt(&path, &e))
}

/// What a snapshot restore did.
#[derive(Debug, Clone, Serialize)]
pub struct Restored {
    /// The snapshot restored.
    pub restored: SnapshotInfo,
    /// The `pre-restore` snapshot taken first.
    pub pre_restore: SnapshotInfo,
}

/// Restores snapshot `id` into `profile`: refuses while another process
/// uses the profile, verifies every file, takes a `pre-restore` snapshot,
/// then replaces the files (databases through the SQLite backup API).
///
/// A session of this same process calling it (`/snapshot restore`) runs
/// between turns, so its own runtime record does not block the restore.
///
/// # Errors
///
/// `snapshot.not_found` (66), `profile.in_use` (75), `backup.corrupt`
/// (65), `backup.io`.
pub fn restore(profile: &ProfileDir, id: &str) -> Result<Restored, Error> {
    let dir = snapshots_dir(profile).join(id);
    if !id.starts_with("snap_") || !dir.join(INFO_FILE).is_file() {
        return Err(Error::new(
            &codes::SNAPSHOT_NOT_FOUND,
            format!("profile '{}' has no snapshot '{id}'", profile.name()),
        )
        .with_hint("list them with: prometheus snapshots list"));
    }
    ensure_unused_by_others(profile)?;
    let info = read_info(&dir)?;
    let files = dir.join(FILES_DIR);
    for member in &info.members {
        let path = members::on_disk(&files, &member.path);
        let (size, sha256) = fsutil::hash_file(&path)?;
        if size != member.size || sha256 != member.sha256 {
            return Err(Error::new(
                &codes::BACKUP_CORRUPT,
                format!("snapshot {id}: {} does not match its manifest", member.path),
            )
            .with_hint("nothing was restored"));
        }
    }
    let pre_restore = create(
        profile,
        Reason::PreRestore,
        Some(&format!("before restoring {id}")),
    )?;
    let staging = crate::create::Staging::new(&snapshots_dir(profile))?;
    for member in &info.members {
        // Restore from a copy: the snapshot itself stays intact.
        let staged = members::on_disk(staging.path(), &member.path);
        if let Some(parent) = staged.parent() {
            private_fs::create_dir(parent).map_err(|e| io_error(parent, &e))?;
        }
        let source = members::on_disk(&files, &member.path);
        private_fs::copy_file(&source, &staged).map_err(|e| io_error(&source, &e))?;
        let target = members::on_disk(profile.path(), &member.path);
        apply::member(&staged, &target, &member.path)?;
    }
    Ok(Restored {
        restored: info,
        pre_restore,
    })
}

fn ensure_unused_by_others(profile: &ProfileDir) -> Result<(), Error> {
    for role in [Role::Daemon, Role::Chat] {
        if let Some(record) = pmt_core::runtime::live_record(profile.home(), role)?
            && record.pid != std::process::id()
            && record.profiles.iter().any(|p| p == profile.name().as_str())
        {
            return Err(Error::new(
                &codes::PROFILE_IN_USE,
                format!(
                    "profile '{}' is in use by pid {}",
                    profile.name(),
                    record.pid
                ),
            )
            .with_hint("stop it, or run /snapshot restore inside that session"));
        }
    }
    Ok(())
}

/// Removes the oldest snapshots bounded by `snapshots.keep` (manual and
/// pre-restore) beyond `keep`; returns the removed ids.
///
/// # Errors
///
/// `backup.io`.
pub fn prune(profile: &ProfileDir, keep: usize) -> Result<Vec<String>, Error> {
    let mut removed = Vec::new();
    let prunable = list(profile)?
        .into_iter()
        .filter(|s| s.reason.kept_by_snapshots_keep())
        .skip(keep);
    for snapshot in prunable {
        let dir = snapshots_dir(profile).join(&snapshot.id);
        std::fs::remove_dir_all(&dir).map_err(|e| io_error(&dir, &e))?;
        removed.push(snapshot.id);
    }
    Ok(removed)
}

fn snapshots_dir(profile: &ProfileDir) -> PathBuf {
    profile.path().join("snapshots")
}

fn corrupt(path: &Path, e: &dyn std::fmt::Display) -> Error {
    Error::new(&codes::BACKUP_CORRUPT, format!("{}: {e}", path.display()))
}
