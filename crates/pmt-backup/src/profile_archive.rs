//! Portable profile archives (spec: configuration-and-profiles, Profile
//! lifecycle commands; Profile export excludes credentials by default):
//! `profile export` writes a `.tar.zst` holding the profile's config,
//! identity file, memory, projects, skills, themes, scripts and cron job
//! definitions, never its secrets, `state.db`, blobs, logs or backups;
//! sessions join with `--with-sessions` and secrets, sealed under a
//! passphrase ([`crate::sealed`]), with `--with-secrets`. `profile import`
//! verifies every member against the archive's manifest before it creates
//! the profile, and only ever writes members of that allowed set.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Read as _, Write as _};
use std::path::{Path, PathBuf};

use pmt_config::{Home, ProfileDir};
use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use serde::{Deserialize, Serialize};

use crate::fsutil::hash_file;
use crate::manifest::Member;
use crate::{members, sqlite};

/// The manifest's entry name and its document shape.
pub const MANIFEST: &str = "profile-export.json";
/// `prometheus.profile_export.v1`.
pub const SCHEMA: &str = "prometheus.profile_export.v1";
/// The built-in memory entries (they live in `state.db`), as JSON.
pub const MEMORY_ENTRY: &str = "memory.json";
/// The sealed secrets.
pub const SECRETS_ENTRY: &str = "secrets.sealed";
/// The projects and the active project (they live in `state.db`), as JSON.
pub const PROJECTS_ENTRY: &str = "projects.json";

/// Profile files every export holds.
const FILES: &[&str] = &["config.toml", "SOUL.md"];
/// Profile directories every export holds.
const DIRS: &[&str] = &["memories", "skills", "themes", "scripts", "cron"];
/// Session directories `--with-sessions` adds (with `state.db`).
const SESSION_DIRS: &[&str] = &["blobs", "trajectories"];
/// The session database.
const DATABASE: &str = "state.db";

/// What a profile archive holds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportManifest {
    /// [`SCHEMA`].
    pub schema: String,
    /// The exported profile.
    pub profile: String,
    /// When (RFC 3339, UTC).
    pub created_at: String,
    /// The version of prometheus that wrote it.
    pub prometheus_version: String,
    /// Sessions are included.
    pub with_sessions: bool,
    /// Sealed secrets are included.
    pub with_secrets: bool,
    /// Every member but the manifest, sorted by path.
    pub members: Vec<Member>,
}

/// What `export` adds besides the profile's files.
#[derive(Debug, Default)]
pub struct ExportOptions {
    /// Include `state.db` (an online copy), blobs and trajectories.
    pub with_sessions: bool,
    /// The memory entries as JSON.
    pub memory: Option<Vec<u8>>,
    /// The projects as JSON.
    pub projects: Option<Vec<u8>>,
    /// The sealed secrets ([`crate::sealed::seal`]).
    pub sealed_secrets: Option<Vec<u8>>,
}

/// Writes `profile` to `output`; returns the manifest.
///
/// # Errors
///
/// `backup.io` when a file cannot be read or the archive written.
pub fn export(
    profile: &ProfileDir,
    output: &Path,
    options: &ExportOptions,
    version: &str,
) -> Result<ExportManifest, Error> {
    let staging = output.with_extension("staging");
    private_fs::create_dir(&staging).map_err(|e| io(&staging, &e))?;
    let result = write_export(profile, output, options, version, &staging);
    let _ = std::fs::remove_dir_all(&staging);
    result
}

fn write_export(
    profile: &ProfileDir,
    output: &Path,
    options: &ExportOptions,
    version: &str,
    staging: &Path,
) -> Result<ExportManifest, Error> {
    let mut sources: BTreeMap<String, PathBuf> =
        profile_members(profile.path(), options.with_sessions)
            .into_iter()
            .map(|relative| {
                (
                    relative.clone(),
                    members::on_disk(profile.path(), &relative),
                )
            })
            .collect();
    if options.with_sessions && profile.path().join(DATABASE).is_file() {
        let copy = staging.join(DATABASE);
        sqlite::copy(&profile.path().join(DATABASE), &copy)?;
        sources.insert(DATABASE.to_owned(), copy);
    }
    for (name, bytes) in [
        (MEMORY_ENTRY, &options.memory),
        (PROJECTS_ENTRY, &options.projects),
        (SECRETS_ENTRY, &options.sealed_secrets),
    ] {
        if let Some(bytes) = bytes {
            let path = staging.join(name);
            private_fs::write_atomic(&path, bytes).map_err(|e| io(&path, &e))?;
            sources.insert(name.to_owned(), path);
        }
    }
    let mut manifest = ExportManifest {
        schema: SCHEMA.to_owned(),
        profile: profile.name().to_string(),
        created_at: jiff::Timestamp::now().to_string(),
        prometheus_version: version.to_owned(),
        with_sessions: options.with_sessions,
        with_secrets: options.sealed_secrets.is_some(),
        members: Vec::new(),
    };
    for (relative, source) in &sources {
        let (size, sha256) = hash_file(source)?;
        manifest.members.push(Member {
            path: relative.clone(),
            size,
            sha256,
        });
    }
    write_tar(output, &manifest, &sources)?;
    Ok(manifest)
}

/// The profile's own files an export holds, profile-relative.
fn profile_members(root: &Path, with_sessions: bool) -> Vec<String> {
    let mut found: Vec<String> = FILES
        .iter()
        .filter(|f| root.join(f).is_file())
        .map(|f| (*f).to_owned())
        .collect();
    let dirs = DIRS
        .iter()
        .chain(SESSION_DIRS.iter().filter(|_| with_sessions));
    for dir in dirs {
        let (files, _) = members::walk(&root.join(dir), &|_| true);
        found.extend(files.into_iter().map(|f| format!("{dir}/{f}")));
    }
    found
}

fn write_tar(
    output: &Path,
    manifest: &ExportManifest,
    sources: &BTreeMap<String, PathBuf>,
) -> Result<(), Error> {
    let partial = output.with_extension("partial");
    let file = private_fs::new_file_options()
        .open(&partial)
        .map_err(|e| io(&partial, &e))?;
    let encoder = zstd::Encoder::new(BufWriter::new(file), 3).map_err(|e| io(&partial, &e))?;
    let mut tar = tar::Builder::new(encoder);
    let json = serde_json::to_vec_pretty(manifest).expect("a manifest serializes");
    append(
        &mut tar,
        MANIFEST,
        &mut json.as_slice(),
        json.len() as u64,
        &partial,
    )?;
    for (relative, source) in sources {
        let mut file = File::open(source).map_err(|e| io(source, &e))?;
        let size = file.metadata().map_err(|e| io(source, &e))?.len();
        append(&mut tar, relative, &mut file, size, &partial)?;
    }
    let mut out = tar
        .into_inner()
        .and_then(zstd::Encoder::finish)
        .map_err(|e| io(&partial, &e))?;
    out.flush().map_err(|e| io(&partial, &e))?;
    drop(out);
    std::fs::rename(&partial, output).map_err(|e| io(output, &e))
}

fn append<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    name: &str,
    data: &mut impl std::io::Read,
    size: u64,
    partial: &Path,
) -> Result<(), Error> {
    let mut header = tar::Header::new_gnu();
    header.set_size(size);
    header.set_mode(0o600);
    header.set_entry_type(tar::EntryType::Regular);
    tar.append_data(&mut header, name, data)
        .map_err(|e| io(partial, &e))
}

/// A verified archive unpacked into a staging directory (removed on
/// drop), ready to become a profile.
#[derive(Debug)]
pub struct Staged {
    dir: PathBuf,
    /// The archive's manifest.
    pub manifest: ExportManifest,
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Unpacks and verifies `archive` under `home`'s `run/` directory; nothing
/// is written to any profile yet.
///
/// # Errors
///
/// `profile.archive_invalid` (65) for anything but a verified archive of
/// allowed members; `backup.io`.
pub fn stage(home: &Home, archive: &Path) -> Result<Staged, Error> {
    let dir = home
        .root()
        .join("run")
        .join(format!("profile-import-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    private_fs::create_dir(&dir).map_err(|e| io(&dir, &e))?;
    // Built first so the directory is removed on every exit.
    let mut staged = Staged {
        manifest: ExportManifest::default(),
        dir,
    };
    staged.manifest = extract(archive, &staged.dir)?;
    Ok(staged)
}

impl Staged {
    /// A member the caller restores itself ([`MEMORY_ENTRY`],
    /// [`PROJECTS_ENTRY`], [`SECRETS_ENTRY`]).
    ///
    /// # Errors
    ///
    /// `backup.io`.
    pub fn read(&self, entry: &str) -> Result<Option<Vec<u8>>, Error> {
        if !self.manifest.members.iter().any(|m| m.path == entry) {
            return Ok(None);
        }
        read(&members::on_disk(&self.dir, entry)).map(Some)
    }

    /// Creates profile `name` (default the exported profile's name) from
    /// the staged files.
    ///
    /// # Errors
    ///
    /// `profile.exists`, name errors, `backup.io`.
    pub fn install(&self, home: &Home, name: Option<&str>) -> Result<ProfileDir, Error> {
        let name = name.unwrap_or(&self.manifest.profile);
        let profile = home.create_profile(name, None)?;
        for member in &self.manifest.members {
            let staged = members::on_disk(&self.dir, &member.path);
            match member.path.as_str() {
                MEMORY_ENTRY | PROJECTS_ENTRY | SECRETS_ENTRY => {}
                DATABASE => sqlite::restore(&staged, &profile.path().join(DATABASE))?,
                relative => {
                    let target = members::on_disk(profile.path(), relative);
                    if let Some(parent) = target.parent() {
                        private_fs::create_dir(parent).map_err(|e| io(parent, &e))?;
                    }
                    private_fs::copy_file(&staged, &target).map_err(|e| io(&target, &e))?;
                }
            }
        }
        Ok(profile)
    }
}

/// Whether an import may write `relative`.
fn importable(relative: &str) -> bool {
    let top = relative.split('/').next().unwrap_or_default();
    FILES.contains(&relative)
        || [MEMORY_ENTRY, PROJECTS_ENTRY, SECRETS_ENTRY, DATABASE].contains(&relative)
        || (relative.contains('/') && (DIRS.contains(&top) || SESSION_DIRS.contains(&top)))
}

fn extract(archive: &Path, staging: &Path) -> Result<ExportManifest, Error> {
    let file = File::open(archive).map_err(|e| io(archive, &e))?;
    let decoder = zstd::Decoder::new(file).map_err(|e| invalid(&e))?;
    let mut tar = tar::Archive::new(decoder);
    let mut manifest: Option<ExportManifest> = None;
    let mut seen: BTreeMap<String, (u64, String)> = BTreeMap::new();
    for entry in tar.entries().map_err(|e| invalid(&e))? {
        let mut entry = entry.map_err(|e| invalid(&e))?;
        let path = entry.path().map_err(|e| invalid(&e))?.into_owned();
        let relative = members::safe_relative(&path)
            .ok_or_else(|| invalid(&format!("unsafe member path {}", path.display())))?;
        if !entry.header().entry_type().is_file() {
            return Err(invalid(&format!("{relative} is not a regular file")));
        }
        if relative == MANIFEST {
            let mut text = String::new();
            entry.read_to_string(&mut text).map_err(|e| invalid(&e))?;
            manifest = Some(serde_json::from_str(&text).map_err(|e| invalid(&e))?);
            continue;
        }
        if !importable(&relative) {
            return Err(invalid(&format!(
                "{relative} is not a profile archive member"
            )));
        }
        let target = members::on_disk(staging, &relative);
        if let Some(parent) = target.parent() {
            private_fs::create_dir(parent).map_err(|e| io(parent, &e))?;
        }
        let mut out = private_fs::new_file_options()
            .open(&target)
            .map_err(|e| io(&target, &e))?;
        std::io::copy(&mut entry, &mut out).map_err(|e| invalid(&e))?;
        drop(out);
        if seen.insert(relative.clone(), hash_file(&target)?).is_some() {
            return Err(invalid(&format!("{relative} appears twice")));
        }
    }
    let manifest = manifest.ok_or_else(|| invalid(&"no profile-export.json manifest"))?;
    verify(&manifest, &seen)?;
    Ok(manifest)
}

fn verify(manifest: &ExportManifest, seen: &BTreeMap<String, (u64, String)>) -> Result<(), Error> {
    if manifest.schema != SCHEMA {
        return Err(invalid(&format!(
            "unsupported archive schema {}",
            manifest.schema
        )));
    }
    if manifest.members.len() != seen.len() {
        return Err(invalid(&"the members differ from the manifest"));
    }
    for member in &manifest.members {
        match seen.get(&member.path) {
            Some((size, sha)) if *size == member.size && *sha == member.sha256 => {}
            _ => {
                return Err(invalid(&format!(
                    "{} does not match the manifest",
                    member.path
                )));
            }
        }
    }
    Ok(())
}

fn read(path: &Path) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(|e| io(path, &e))
}

fn io(path: &Path, e: &std::io::Error) -> Error {
    Error::io(&codes::BACKUP_IO, path, e)
}

fn invalid(e: &dyn std::fmt::Display) -> Error {
    Error::new(
        &codes::PROFILE_ARCHIVE_INVALID,
        format!("not a valid profile archive: {e}"),
    )
}
