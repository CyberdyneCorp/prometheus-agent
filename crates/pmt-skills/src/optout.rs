//! Bundled skill opt-out (spec: skills, Bundled skill opt-out and official
//! catalog repair).
//!
//! `skills.include_bundled = false` hides the bundled tier from every
//! listing, index and slash command of the profile. It is a setting, not a
//! deletion, so it holds across upgrades: a release with new bundled skills
//! adds nothing to an opted-out profile. The essential skill stays
//! (`crate::roots`). Ejected copies are the user's: opting out with
//! `--remove` deletes only the copies still byte-identical to their
//! bundled origin.

use std::fs;
use std::path::{Path, PathBuf};

use pmt_config::ProfileDir;
use pmt_core::error::Error;

use crate::catalog::Catalog;
use crate::digest;
use crate::lockfile::{Contents, Entry, LockFile, io};
use crate::roots::{Tier, profile_skills};

/// Sets `skills.include_bundled` of the profile.
///
/// # Errors
///
/// Config write errors.
pub fn set_included(profile: &ProfileDir, included: bool) -> Result<(), Error> {
    profile.config_file().set(
        "skills.include_bundled",
        if included { "true" } else { "false" },
    )
}

/// An ejected copy of a bundled skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copy {
    /// The skill.
    pub name: String,
    /// Its directory.
    pub path: PathBuf,
    /// The bundled origin's digest recorded at eject.
    pub origin: String,
    /// The copy's digest now.
    pub current: String,
}

impl Copy {
    /// Whether the copy still equals its origin byte for byte.
    #[must_use]
    pub fn identical(&self) -> bool {
        self.origin == self.current
    }
}

fn ejected_entries(profile_dir: &Path) -> Vec<Entry> {
    match LockFile::in_dir(&profile_skills(profile_dir)).read() {
        Contents::Entries(entries) => entries
            .into_iter()
            .filter(|e| e.bundled_path().is_some())
            .collect(),
        Contents::Unreadable(_) => Vec::new(),
    }
}

/// Every ejected copy still on disk (`catalog` locates them).
#[must_use]
pub fn ejected(profile_dir: &Path, catalog: &Catalog) -> Vec<Copy> {
    ejected_entries(profile_dir)
        .into_iter()
        .filter_map(|entry| {
            let path = catalog
                .skills
                .iter()
                .find(|s| s.name == entry.name && s.tier == Tier::Profile)
                .and_then(|s| s.dir.path().map(Path::to_path_buf))?;
            Some(Copy {
                current: digest::dir(&path),
                name: entry.name,
                path,
                origin: entry.content_digest,
            })
        })
        .collect()
}

/// `prometheus skills list-modified`: ejected copies that differ from
/// their recorded origin.
#[must_use]
pub fn modified(profile_dir: &Path, catalog: &Catalog) -> Vec<Copy> {
    ejected(profile_dir, catalog)
        .into_iter()
        .filter(|c| !c.identical())
        .collect()
}

/// Deletes the unedited ejected copies and their lock entries; returns
/// their names. Edited copies, hub and user skills are kept.
///
/// # Errors
///
/// `skill.io`.
pub fn remove_identical(profile_dir: &Path, catalog: &Catalog) -> Result<Vec<String>, Error> {
    let skills = profile_skills(profile_dir);
    let lock = LockFile::in_dir(&skills);
    let mut removed = Vec::new();
    for copy in ejected(profile_dir, catalog)
        .into_iter()
        .filter(Copy::identical)
    {
        if !crate::manage::inside(&skills, &copy.path) {
            continue;
        }
        fs::remove_dir_all(&copy.path).map_err(|e| io(&copy.path, &e))?;
        lock.remove(&copy.name)?;
        removed.push(copy.name);
    }
    Ok(removed)
}
