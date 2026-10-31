//! Ejecting bundled skills (spec: skills, Bundled skills are read-only and
//! ejectable (upgrade)).
//!
//! `prometheus skills eject <name>` copies a bundled skill into
//! `<profile>/skills/<category>/<name>/` and records the bundled tree's
//! digest in `skills.lock`; the copy then shadows the bundled one and is
//! never touched by an upgrade. `outdated` lists ejected skills whose
//! bundled origin changed since, `diff` shows how the copy differs from the
//! current bundled version and `reset` drops the copy (and its record) so
//! the name resolves to the bundled skill again.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use similar::TextDiff;

use crate::bundled::Bundled;
use crate::catalog::Catalog;
use crate::lockfile::{self, BUNDLED_SOURCE, Contents, Entry, LockFile, Trust, io};
use crate::roots::{RootInputs, Roots, Tier, profile_skills};
use crate::scan::PatternScanner;
use crate::source::SkillDir;

/// The profile's skills outside a session, as seen from `cwd` (`None`:
/// no project tier): what the `prometheus skills` commands work on.
#[must_use]
pub fn profile_catalog(
    scope: &ProfileScope,
    cwd: Option<&Path>,
    user_home: Option<&Path>,
    bundled: Arc<Bundled>,
) -> Catalog {
    let config = scope.config();
    let roots = Roots::resolve(&RootInputs {
        config: &config.skills,
        profile_dir: scope.dir(),
        cwd,
        user_home,
        plugins: &[],
        bundled,
    });
    let lock = LockFile::in_dir(&profile_skills(scope.dir())).read();
    Catalog::load(&roots, &lock, &PatternScanner)
}

/// A finished eject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ejected {
    /// The skill.
    pub name: String,
    /// Where the copy is.
    pub path: PathBuf,
}

/// An ejected skill whose bundled origin changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outdated {
    /// The skill.
    pub name: String,
    /// Digest recorded at eject.
    pub ejected: String,
    /// Digest of the bundled version now (`None`: no longer bundled).
    pub bundled: Option<String>,
}

fn not_bundled(name: &str) -> Error {
    Error::new(
        &codes::SKILL_NOT_FOUND,
        format!("no bundled skill named '{name}'"),
    )
}

/// The bundled skill `name` of `catalog` (winning or shadowed).
fn bundled_prefix(catalog: &Catalog, name: &str) -> Option<(String, String, Arc<Bundled>)> {
    catalog
        .skills
        .iter()
        .chain(&catalog.shadowed)
        .filter(|s| s.tier == Tier::Bundled && (s.name == name || s.dir_name == name))
        .find_map(|s| match &s.dir {
            SkillDir::Bundled { tree, prefix } => {
                Some((prefix.clone(), s.category.clone(), Arc::clone(tree)))
            }
            SkillDir::Disk(_) => None,
        })
}

/// Copies bundled skill `name` into the profile at `profile_dir`.
///
/// # Errors
///
/// `skill.not_found` (no such bundled skill), `skill.invalid` (the
/// profile already has a skill of that name), `skill.io`.
pub fn eject(profile_dir: &Path, catalog: &Catalog, name: &str) -> Result<Ejected, Error> {
    let (prefix, category, tree) =
        bundled_prefix(catalog, name).ok_or_else(|| not_bundled(name))?;
    let shadowing = catalog
        .skills
        .iter()
        .find(|s| s.name == name && s.tier != Tier::Bundled);
    if let Some(existing) = shadowing {
        return Err(Error::new(
            &codes::SKILL_INVALID,
            format!(
                "skill '{name}' already exists in the {} tier at {}",
                existing.tier.as_str(),
                existing.dir.display()
            ),
        ));
    }
    let skills = profile_skills(profile_dir);
    let dir_name = prefix.rsplit('/').next().unwrap_or(name);
    let target = skills.join(&category).join(dir_name);
    if target.exists() {
        return Err(Error::new(
            &codes::SKILL_INVALID,
            format!("{} already exists", target.display()),
        ));
    }
    for rel in tree.files_in(&prefix) {
        let bytes = tree.read(&prefix, &rel).unwrap_or_default();
        let path = rel.split('/').fold(target.clone(), |p, c| p.join(c));
        if let Some(parent) = path.parent() {
            private_fs::create_dir(parent).map_err(|e| io(parent, &e))?;
        }
        private_fs::write_atomic(&path, bytes).map_err(|e| io(&path, &e))?;
    }
    LockFile::in_dir(&skills).upsert(Entry {
        name: name.to_owned(),
        source: format!("{BUNDLED_SOURCE}{prefix}"),
        content_digest: tree.digest(&prefix),
        trust: Some(Trust::Builtin),
        installed_at: lockfile::now(),
    })?;
    Ok(Ejected {
        name: name.to_owned(),
        path: target,
    })
}

/// Ejected skills whose bundled origin changed since the eject.
#[must_use]
pub fn outdated(profile_dir: &Path, bundled: &Bundled) -> Vec<Outdated> {
    let Contents::Entries(entries) = LockFile::in_dir(&profile_skills(profile_dir)).read() else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|entry| {
            let prefix = entry.bundled_path()?.to_owned();
            let now = (!bundled.files_in(&prefix).is_empty()).then(|| bundled.digest(&prefix));
            (now.as_deref() != Some(entry.content_digest.as_str())).then_some(Outdated {
                name: entry.name,
                ejected: entry.content_digest,
                bundled: now,
            })
        })
        .collect()
}

fn ejected_entry(profile_dir: &Path, name: &str) -> Result<Entry, Error> {
    let lock = LockFile::in_dir(&profile_skills(profile_dir));
    match lock.read() {
        Contents::Entries(entries) => entries
            .into_iter()
            .find(|e| e.name == name && e.bundled_path().is_some())
            .ok_or_else(|| {
                Error::new(
                    &codes::SKILL_NOT_FOUND,
                    format!("skill '{name}' was not ejected from the bundled skills"),
                )
            }),
        Contents::Unreadable(why) => Err(Error::new(&codes::SKILL_IO, why)),
    }
}

/// A unified diff from the current bundled version to the ejected copy of
/// `name` (empty when equal).
///
/// # Errors
///
/// `skill.not_found` when `name` was not ejected.
pub fn diff(
    profile_dir: &Path,
    catalog: &Catalog,
    bundled: &Bundled,
    name: &str,
) -> Result<String, Error> {
    let entry = ejected_entry(profile_dir, name)?;
    let prefix = entry.bundled_path().unwrap_or_default();
    let copy = catalog
        .skills
        .iter()
        .find(|s| s.name == name && s.tier == Tier::Profile)
        .and_then(|s| s.dir.path().map(Path::to_path_buf))
        .ok_or_else(|| {
            Error::new(
                &codes::SKILL_NOT_FOUND,
                format!("the ejected copy of '{name}' is gone"),
            )
        })?;
    let mut files: Vec<String> = bundled.files_in(prefix);
    files.extend(crate::layout::walk(&copy));
    files.sort();
    files.dedup();
    let mut out = String::new();
    for rel in files {
        let old = bundled
            .read(prefix, &rel)
            .map(String::from_utf8_lossy)
            .unwrap_or_default();
        let new = fs::read(copy.join(&rel))
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        if old != new {
            let text = TextDiff::from_lines(old.as_ref(), new.as_str())
                .unified_diff()
                .header(&format!("bundled/{rel}"), &format!("ejected/{rel}"))
                .to_string();
            let _ = write!(out, "{text}");
        }
    }
    Ok(out)
}

/// Removes the ejected copy of `name` and its record.
///
/// # Errors
///
/// `skill.not_found` when `name` was not ejected, `skill.io`.
pub fn reset(profile_dir: &Path, catalog: &Catalog, name: &str) -> Result<(), Error> {
    ejected_entry(profile_dir, name)?;
    let skills = profile_skills(profile_dir);
    let copy = catalog
        .skills
        .iter()
        .find(|s| s.name == name && s.tier == Tier::Profile)
        .and_then(|s| s.dir.path())
        .filter(|dir| crate::manage::inside(&skills, dir));
    if let Some(dir) = copy {
        fs::remove_dir_all(dir).map_err(|e| io(dir, &e))?;
    }
    LockFile::in_dir(&skills).remove(name)
}
