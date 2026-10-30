//! `prometheus backup restore` (spec: configuration-and-profiles, Home
//! restore). [`plan`] extracts the whole archive into a private staging
//! directory and checks every member against the manifest before anything
//! in the home changes; [`Plan::apply`] then snapshots the state it is
//! about to replace and puts the members in place.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read as _};
use std::path::Path;

use pmt_config::{Home, ProfileName};
use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use pmt_core::runtime::{self, Role};
use serde::Serialize;

use crate::create::Staging;
use crate::fsutil::{self, Hashing, io_error};
use crate::manifest::{self, Manifest};
use crate::snapshot::{self, Reason, SnapshotInfo};
use crate::{apply, members, sqlite};

/// A verified archive, staged and ready to restore.
pub struct Plan {
    home: Home,
    staging: Staging,
    manifest: Manifest,
    selected: Vec<String>,
    affected: BTreeSet<String>,
    /// Some target file already exists with different contents: the
    /// caller asks for confirmation (or needs `--force`).
    pub holds_data: bool,
    /// `restoring an older backup: ...` lines for the user.
    pub warnings: Vec<String>,
}

/// What a restore did.
#[derive(Debug, Clone, Serialize)]
pub struct Restored {
    /// Members put in place.
    pub members: usize,
    /// Profiles touched.
    pub profiles: Vec<String>,
    /// The `pre-restore` snapshots taken first.
    pub snapshots: Vec<SnapshotInfo>,
}

impl std::fmt::Debug for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plan")
            .field("selected", &self.selected.len())
            .field("affected", &self.affected)
            .field("holds_data", &self.holds_data)
            .finish_non_exhaustive()
    }
}

/// Verifies `archive` and prepares restoring it into `home` (only the
/// subtree of `profile` when given).
///
/// # Errors
///
/// `backup.corrupt` (65) for an unreadable archive or any member that does
/// not match the manifest; `profile.not_found` when `profile` is not in
/// the archive; `profile.in_use` (75) when a running process uses an
/// affected profile; `backup.io`. Nothing in the home has changed then.
pub fn plan(home: &Home, archive: &Path, profile: Option<&ProfileName>) -> Result<Plan, Error> {
    let backups = home.root().join("backups");
    private_fs::create_dir(&backups).map_err(|e| io_error(&backups, &e))?;
    let staging = Staging::new(&backups)?;
    let manifest = extract_verified(archive, staging.path())?;
    let selected: Vec<String> = manifest
        .members
        .iter()
        .map(|m| m.path.clone())
        .filter(|path| members::allowed(path))
        .filter(|path| profile.is_none_or(|p| members::profile_of(path) == Some(p.as_str())))
        .collect();
    if let Some(profile) = profile
        && selected.is_empty()
    {
        return Err(Error::new(
            &codes::PROFILE_NOT_FOUND,
            format!("the backup holds no profile '{profile}'"),
        ));
    }
    let affected: BTreeSet<String> = selected
        .iter()
        .filter_map(|path| members::profile_of(path).map(str::to_owned))
        .collect();
    ensure_unused(
        home,
        &affected,
        selected.iter().any(|p| members::profile_of(p).is_none()),
    )?;
    let staged = |path: &str| members::on_disk(&staging.path().join("files"), path);
    let target = |path: &str| members::on_disk(home.root(), path);
    let holds_data = manifest
        .members
        .iter()
        .filter(|m| selected.contains(&m.path))
        .any(|m| {
            let existing = target(&m.path);
            existing.exists()
                && fsutil::hash_file(&existing).map_or(true, |(_, hash)| hash != m.sha256)
        });
    let warnings = selected
        .iter()
        .filter(|path| path.ends_with("/state.db") && target(path).exists())
        .filter_map(|path| older_backup_warning(&target(path), &staged(path)))
        .collect();
    Ok(Plan {
        home: home.clone(),
        staging,
        manifest,
        selected,
        affected,
        holds_data,
        warnings,
    })
}

impl Plan {
    /// The verified manifest.
    #[must_use]
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Takes a `pre-restore` snapshot of every affected existing profile,
    /// then puts each member in place.
    ///
    /// # Errors
    ///
    /// `backup.io`, snapshot errors.
    pub fn apply(self) -> Result<Restored, Error> {
        let mut snapshots = Vec::new();
        for name in &self.affected {
            let name = ProfileName::parse(name)?;
            if self.home.profile_path(&name).is_dir() && !self.home.tombstone(&name).exists() {
                let profile = self.home.existing_profile(&name)?;
                snapshots.push(snapshot::create(&profile, Reason::PreRestore, None)?);
            }
            // Restoring a profile is an explicit way back from deletion.
            let tombstone = self.home.tombstone(&name);
            if tombstone.exists() {
                std::fs::remove_file(&tombstone).map_err(|e| io_error(&tombstone, &e))?;
            }
        }
        let files = self.staging.path().join("files");
        for path in &self.selected {
            let staged = members::on_disk(&files, path);
            apply::member(&staged, &members::on_disk(self.home.root(), path), path)?;
        }
        Ok(Restored {
            members: self.selected.len(),
            profiles: self.affected.iter().cloned().collect(),
            snapshots,
        })
    }
}

fn ensure_unused(home: &Home, affected: &BTreeSet<String>, home_files: bool) -> Result<(), Error> {
    for name in affected {
        let name =
            ProfileName::parse(name).map_err(|_| corrupt(&format!("invalid profile '{name}'")))?;
        pmt_config::profiles::ensure_unused(home.root(), &name)?;
    }
    if home_files && let Some(record) = runtime::live_record(home.root(), Role::Daemon)? {
        return Err(Error::new(
            &codes::PROFILE_IN_USE,
            format!("the gateway daemon (pid {}) is running", record.pid),
        )
        .with_hint("stop it with: prometheus daemon stop"));
    }
    Ok(())
}

fn older_backup_warning(current: &Path, restored: &Path) -> Option<String> {
    let (now_sessions, now_messages) = sqlite::activity(current);
    let (old_sessions, old_messages) = sqlite::activity(restored);
    let sessions = now_sessions.saturating_sub(old_sessions);
    let messages = now_messages.saturating_sub(old_messages);
    (sessions > 0 || messages > 0).then(|| {
        format!(
            "restoring an older backup: {sessions} sessions and {messages} messages newer than the backup will be lost"
        )
    })
}

/// Extracts every member into `<staging>/files/` and checks the archive
/// against its manifest: each member present once, with the manifest's
/// size and hash, and nothing unlisted.
fn extract_verified(archive: &Path, staging: &Path) -> Result<Manifest, Error> {
    let file = File::open(archive).map_err(|e| io_error(archive, &e))?;
    let decoder = zstd::Decoder::new(file).map_err(|e| corrupt(&e.to_string()))?;
    let mut tar = tar::Archive::new(decoder);
    let mut seen: BTreeMap<String, (u64, String)> = BTreeMap::new();
    let mut manifest = None;
    for entry in tar.entries().map_err(|e| corrupt(&e.to_string()))? {
        let mut entry = entry.map_err(|e| corrupt(&e.to_string()))?;
        let path = entry.path().map_err(|e| corrupt(&e.to_string()))?;
        let relative = members::safe_relative(&path)
            .ok_or_else(|| corrupt(&format!("unsafe member path {}", path.display())))?;
        if !entry.header().entry_type().is_file() {
            return Err(corrupt(&format!("{relative} is not a regular file")));
        }
        if relative == manifest::ARCHIVE_ENTRY {
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .map_err(|e| corrupt(&e.to_string()))?;
            manifest =
                Some(serde_json::from_str::<Manifest>(&text).map_err(|e| corrupt(&e.to_string()))?);
            continue;
        }
        let target = members::on_disk(&staging.join("files"), &relative);
        let written = write_staged(&mut entry, &target)?;
        if seen.insert(relative.clone(), written).is_some() {
            return Err(corrupt(&format!("{relative} appears twice")));
        }
    }
    let manifest = manifest.ok_or_else(|| corrupt("the archive has no manifest"))?;
    check_against(&manifest, &seen)?;
    Ok(manifest)
}

fn write_staged(entry: &mut impl io::Read, target: &Path) -> Result<(u64, String), Error> {
    if let Some(parent) = target.parent() {
        private_fs::create_dir(parent).map_err(|e| io_error(parent, &e))?;
    }
    let mut out = private_fs::new_file_options()
        .open(target)
        .map_err(|e| io_error(target, &e))?;
    let mut reader = Hashing::new(entry);
    io::copy(&mut reader, &mut out).map_err(|e| corrupt(&e.to_string()))?;
    Ok(reader.finish())
}

fn check_against(manifest: &Manifest, seen: &BTreeMap<String, (u64, String)>) -> Result<(), Error> {
    if manifest.format != manifest::FORMAT {
        return Err(corrupt(&format!(
            "unsupported archive format {}",
            manifest.format
        )));
    }
    for member in &manifest.members {
        match seen.get(&member.path) {
            Some((size, hash)) if *size == member.size && *hash == member.sha256 => {}
            Some(_) => {
                return Err(corrupt(&format!(
                    "{} does not match the manifest",
                    member.path
                )));
            }
            None => return Err(corrupt(&format!("{} is missing", member.path))),
        }
    }
    if let Some(extra) = seen
        .keys()
        .find(|p| !manifest.members.iter().any(|m| &m.path == *p))
    {
        return Err(corrupt(&format!("{extra} is not in the manifest")));
    }
    Ok(())
}

fn corrupt(problem: &str) -> Error {
    Error::new(
        &codes::BACKUP_CORRUPT,
        format!("invalid backup archive: {problem}"),
    )
    .with_hint("nothing was restored")
}
