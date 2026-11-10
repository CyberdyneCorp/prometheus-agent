//! Moving skills in and out of `skills/.archive/` (and out of existence,
//! for an explicit purge), each move recorded in the ledger first-class.

use std::path::{Path, PathBuf};

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use pmt_store::Store;
use pmt_store::skill_usage::SkillState;
use serde::Serialize;

use crate::catalog::{Catalog, Provenance};
use crate::ledger::{Actor, Evidence, Ledger};
use crate::roots::Tier;

/// The archive directory below `skills/`.
pub const ARCHIVE_DIR: &str = ".archive";

fn io(path: &Path, e: &std::io::Error) -> Error {
    Error::io(&codes::SKILL_IO, path, e)
}

/// A free archive name for `name`: `<name>`, else `<name>-<UTC stamp>`.
fn archive_slot(skills_dir: &Path, name: &str, now: Timestamp) -> String {
    let plain = format!("{ARCHIVE_DIR}/{name}");
    if !skills_dir.join(ARCHIVE_DIR).join(name).exists() {
        return plain;
    }
    format!("{plain}-{}", now.strftime("%Y%m%dT%H%M%SZ"))
}

/// Moves the skill directory `rel` (relative to `skills/`) into the
/// archive, recording it; the archive path relative to `skills/`.
///
/// # Errors
///
/// `skill.io`.
pub fn archive(
    ledger: &Ledger<'_>,
    store: &Store,
    name: &str,
    rel: &str,
    actor: Actor,
    evidence: &Evidence,
) -> Result<String, Error> {
    let skills_dir = ledger.skills_dir().to_path_buf();
    let target_rel = archive_slot(&skills_dir, name, Timestamp::now());
    let source = skills_dir.join(rel);
    let target = skills_dir.join(&target_rel);
    let moved: Vec<String> = ledger
        .files_below(rel)
        .iter()
        .map(|f| format!("{target_rel}{}", &f[rel.len()..]))
        .collect();
    let before = ledger.capture_dir(rel).with_new(moved);
    if let Some(parent) = target.parent() {
        private_fs::create_dir(parent).map_err(|e| io(parent, &e))?;
    }
    std::fs::rename(&source, &target).map_err(|e| io(&source, &e))?;
    ledger.record(actor, "archive", name, &before, evidence);
    if let Err(e) = store.set_skill_state(name, SkillState::Archived) {
        tracing::warn!(skill = %name, error = %e, "skill state not recorded");
    }
    Ok(target_rel)
}

/// One archived skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Archived {
    /// The skill (the archive directory name without its stamp).
    pub name: String,
    /// The archive directory, relative to `skills/`.
    pub rel: String,
    /// When it was archived (its directory time).
    pub archived_at: Option<String>,
    #[serde(skip)]
    at: Option<Timestamp>,
}

fn base_name(dir_name: &str) -> &str {
    // `<name>-<YYYYMMDDTHHMMSSZ>` from a collision.
    match dir_name.rsplit_once('-') {
        Some((base, stamp)) if stamp.len() == 16 && stamp.ends_with('Z') && stamp.contains('T') => {
            base
        }
        _ => dir_name,
    }
}

/// Every archived skill, oldest first.
#[must_use]
pub fn archived(skills_dir: &Path) -> Vec<Archived> {
    let Ok(entries) = std::fs::read_dir(skills_dir.join(ARCHIVE_DIR)) else {
        return Vec::new();
    };
    let mut out: Vec<Archived> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| {
            let dir_name = e.file_name().to_string_lossy().into_owned();
            let at = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| Timestamp::try_from(t).ok());
            Archived {
                name: base_name(&dir_name).to_owned(),
                rel: format!("{ARCHIVE_DIR}/{dir_name}"),
                archived_at: at.map(|t| t.to_string()),
                at,
            }
        })
        .collect();
    out.sort_by_key(|a| a.at);
    out
}

/// Moves the newest archive of `name` back to `skills/<name>`, refusing a
/// name a hub, plugin or bundled skill now occupies (or any skill).
///
/// # Errors
///
/// `curator.refused`, `skill.not_found`, `skill.io`.
pub fn restore(
    ledger: &Ledger<'_>,
    store: &Store,
    catalog: &Catalog,
    name: &str,
    actor: Actor,
) -> Result<String, Error> {
    if let Some(taken) = catalog.find(name) {
        let owner = match (taken.tier, taken.provenance) {
            (_, Provenance::Hub(_)) => "a hub skill",
            (Tier::Plugin, _) => "a plugin skill",
            (Tier::Bundled, _) | (_, Provenance::Builtin) => "a bundled skill",
            _ => "another skill",
        };
        return Err(Error::new(
            &codes::CURATOR_REFUSED,
            format!(
                "cannot restore '{name}': the name is now taken by {owner} ({})",
                taken.dir.display()
            ),
        ));
    }
    let skills_dir = ledger.skills_dir().to_path_buf();
    let found = archived(&skills_dir)
        .into_iter()
        .rev()
        .find(|a| a.name == name)
        .ok_or_else(|| {
            Error::new(
                &codes::SKILL_NOT_FOUND,
                format!("no archived skill '{name}'"),
            )
        })?;
    let target = skills_dir.join(name);
    if target.exists() {
        return Err(Error::new(
            &codes::CURATOR_REFUSED,
            format!("cannot restore '{name}': {} exists", target.display()),
        ));
    }
    let restored: Vec<String> = ledger
        .files_below(&found.rel)
        .iter()
        .map(|f| format!("{name}{}", &f[found.rel.len()..]))
        .collect();
    let before = ledger.capture_dir(&found.rel).with_new(restored);
    let source = skills_dir.join(&found.rel);
    std::fs::rename(&source, &target).map_err(|e| io(&source, &e))?;
    ledger.record(actor, "restore", name, &before, &Evidence::default());
    if let Err(e) = store.set_skill_state(name, SkillState::Active) {
        tracing::warn!(skill = %name, error = %e, "skill state not recorded");
    }
    Ok(name.to_owned())
}

/// Archived skills older than `age_secs`.
#[must_use]
pub fn expired(skills_dir: &Path, age_secs: u64, now: Timestamp) -> Vec<Archived> {
    let cutoff = now
        .checked_sub(jiff::SignedDuration::from_secs(
            i64::try_from(age_secs).unwrap_or(i64::MAX),
        ))
        .unwrap_or(Timestamp::MIN);
    archived(skills_dir)
        .into_iter()
        .filter(|a| a.at.is_some_and(|at| at < cutoff))
        .collect()
}

/// Deletes archived skills older than `age_secs` (an explicit purge
/// only; a pass never calls it), ledgering each before it goes; the
/// purged ones.
///
/// # Errors
///
/// `skill.io`.
pub fn purge(ledger: &Ledger<'_>, age_secs: u64, now: Timestamp) -> Result<Vec<Archived>, Error> {
    let skills_dir: PathBuf = ledger.skills_dir().to_path_buf();
    let doomed = expired(&skills_dir, age_secs, now);
    for archived in &doomed {
        let before = ledger.capture_dir(&archived.rel);
        let dir = skills_dir.join(&archived.rel);
        std::fs::remove_dir_all(&dir).map_err(|e| io(&dir, &e))?;
        ledger.record(
            Actor::User,
            "purge",
            &archived.name,
            &before,
            &Evidence::default(),
        );
    }
    Ok(doomed)
}
