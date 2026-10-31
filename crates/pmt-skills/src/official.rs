//! Official catalog repair (spec: skills, Bundled skill opt-out and
//! official catalog repair).
//!
//! Official skills ship embedded in the binary (`Bundled::official`) and
//! are installed into the profile as `official/<category>/<name>`. When an
//! install lost its provenance (a lockfile edited or restored from an old
//! backup), `repair` re-records it for every copy that still matches the
//! embedded one; with `restore`, a missing or modified copy is replaced
//! from the embedded catalog after the existing copy is moved to
//! `<profile>/skills/.backups/`.

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use serde::Serialize;

use crate::bundled::Bundled;
use crate::catalog::Catalog;
use crate::digest;
use crate::layout;
use crate::lockfile::{self, Contents, Entry, LockFile, Trust, io};
use crate::roots::{Tier, profile_skills};

/// The `source` prefix of an official install.
pub const OFFICIAL_SOURCE: &str = "official/";

/// What repair did to one skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// The copy matches; its provenance was recorded.
    Recorded,
    /// The copy was missing or modified and was replaced.
    Restored,
    /// No copy in the profile (`--restore` installs it).
    Missing,
    /// The copy differs from the embedded one (`--restore` replaces it).
    Modified,
}

/// One repaired skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Repair {
    /// The skill.
    pub name: String,
    /// `official/<category>/<name>`.
    pub source: String,
    /// What happened.
    pub action: Action,
    /// Where the previous copy was moved (restored only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<PathBuf>,
}

/// The official skills by name: `(name, path in the catalog)`.
#[must_use]
pub fn skills(official: &Bundled) -> Vec<(String, String)> {
    layout::skill_dirs(official.paths())
        .into_iter()
        .map(|prefix| {
            let name = prefix.rsplit('/').next().unwrap_or(&prefix).to_owned();
            (name, prefix)
        })
        .collect()
}

/// What `repair` works on.
#[derive(Debug, Clone, Copy)]
pub struct Options<'a> {
    /// One skill name, or `all`: every official skill the profile holds
    /// (by lock entry or by a profile skill of that name).
    pub target: &'a str,
    /// Replace missing and modified copies.
    pub restore: bool,
}

fn installed(profile_dir: &Path, catalog: &Catalog, name: &str) -> bool {
    let recorded = match LockFile::in_dir(&profile_skills(profile_dir)).read() {
        Contents::Entries(entries) => entries
            .iter()
            .any(|e| e.name == name && e.source.starts_with(OFFICIAL_SOURCE)),
        Contents::Unreadable(_) => false,
    };
    recorded || profile_copy(catalog, name).is_some()
}

fn profile_copy(catalog: &Catalog, name: &str) -> Option<PathBuf> {
    catalog
        .skills
        .iter()
        .chain(&catalog.shadowed)
        .find(|s| s.name == name && s.tier == Tier::Profile)
        .and_then(|s| s.dir.path().map(Path::to_path_buf))
}

/// Repairs the official installs `options.target` names.
///
/// # Errors
///
/// `skill.not_found` (no official skill of that name), `skill.io`.
pub fn repair(
    profile_dir: &Path,
    catalog: &Catalog,
    official: &Bundled,
    options: Options<'_>,
) -> Result<Vec<Repair>, Error> {
    let all = skills(official);
    let chosen: Vec<&(String, String)> = if options.target == "all" {
        all.iter()
            .filter(|(name, _)| installed(profile_dir, catalog, name))
            .collect()
    } else {
        let found: Vec<_> = all.iter().filter(|(n, _)| n == options.target).collect();
        if found.is_empty() {
            return Err(Error::new(
                &codes::SKILL_NOT_FOUND,
                format!("no official skill named '{}'", options.target),
            ));
        }
        found
    };
    chosen
        .into_iter()
        .map(|(name, prefix)| {
            repair_one(
                profile_dir,
                catalog,
                official,
                name,
                prefix,
                options.restore,
            )
        })
        .collect()
}

fn repair_one(
    profile_dir: &Path,
    catalog: &Catalog,
    official: &Bundled,
    name: &str,
    prefix: &str,
    restore: bool,
) -> Result<Repair, Error> {
    let skills = profile_skills(profile_dir);
    let embedded = official.digest(prefix);
    let copy = profile_copy(catalog, name);
    let matches = copy
        .as_deref()
        .is_some_and(|dir| digest::dir(dir) == embedded);
    let mut repair = Repair {
        name: name.to_owned(),
        source: format!("{OFFICIAL_SOURCE}{prefix}"),
        action: Action::Recorded,
        backup: None,
    };
    if !matches {
        if !restore {
            repair.action = if copy.is_some() {
                Action::Modified
            } else {
                Action::Missing
            };
            return Ok(repair);
        }
        if let Some(dir) = &copy {
            repair.backup = Some(back_up(&skills, name, dir)?);
        }
        let target = copy.unwrap_or_else(|| skills.join(prefix));
        write_tree(official, prefix, &target)?;
        repair.action = Action::Restored;
    }
    LockFile::in_dir(&skills).upsert(Entry {
        name: name.to_owned(),
        source: repair.source.clone(),
        content_digest: embedded,
        trust: Some(Trust::Builtin),
        installed_at: lockfile::now(),
    })?;
    Ok(repair)
}

/// Moves `dir` to `<skills>/.backups/<name>-<time>/`.
fn back_up(skills: &Path, name: &str, dir: &Path) -> Result<PathBuf, Error> {
    let backups = skills.join(".backups");
    private_fs::create_dir(&backups).map_err(|e| io(&backups, &e))?;
    let stamp = jiff::Timestamp::now()
        .strftime("%Y%m%dT%H%M%S%.3fZ")
        .to_string();
    let target = backups.join(format!("{name}-{stamp}"));
    std::fs::rename(dir, &target).map_err(|e| io(dir, &e))?;
    Ok(target)
}

/// Writes the embedded files below `prefix` into `target`.
fn write_tree(tree: &Bundled, prefix: &str, target: &Path) -> Result<(), Error> {
    for rel in tree.files_in(prefix) {
        let bytes = tree.read(prefix, &rel).unwrap_or_default();
        let path = rel.split('/').fold(target.to_path_buf(), |p, c| p.join(c));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io(parent, &e))?;
        }
        private_fs::write_atomic(&path, bytes).map_err(|e| io(&path, &e))?;
    }
    Ok(())
}
