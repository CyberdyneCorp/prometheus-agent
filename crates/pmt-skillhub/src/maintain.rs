//! Keeping hub installs honest (spec: skills, Hub discovery, inspection,
//! updates and audit; Skills lockfile and tamper detection): `check`,
//! `update`, `audit`, `uninstall`, `accept`, `reset` and `install
//! --locked`, all driven by `skills.lock`.

use std::path::PathBuf;

use pmt_core::error::{Error, codes};
use pmt_skills::digest;
use pmt_skills::lockfile::{Contents, Entry, LockFile};
use pmt_skills::scan::Verdict;
use serde::Serialize;

use crate::bundle::safe_path;
use crate::hub::Hub;
use crate::ident::Identifier;
use crate::install::{self, Options};
use crate::sources;

/// The hub installs of the profile (ejected bundled copies left out).
///
/// # Errors
///
/// `skill.io` when `skills.lock` does not parse.
pub fn entries(hub: &Hub) -> Result<Vec<Entry>, Error> {
    match LockFile::in_dir(&hub.skills_dir()).read() {
        Contents::Entries(entries) => Ok(entries.into_iter().filter(Entry::is_hub).collect()),
        Contents::Unreadable(why) => Err(Error::new(&codes::SKILL_IO, why)),
    }
}

/// The hub install of `name`.
///
/// # Errors
///
/// `skill.not_hub` when `name` is not a hub install.
pub fn entry(hub: &Hub, name: &str) -> Result<Entry, Error> {
    entries(hub)?
        .into_iter()
        .find(|e| e.name == name)
        .ok_or_else(|| {
            Error::new(
                &codes::SKILL_NOT_HUB,
                format!("'{name}' is not a hub-installed skill (none in skills.lock)"),
            )
        })
}

/// The directory of an entry, `None` when its recorded path is unsafe.
#[must_use]
pub fn install_dir(hub: &Hub, entry: &Entry) -> Option<PathBuf> {
    let rel = entry.path.clone().unwrap_or_else(|| entry.name.clone());
    safe_path(&rel).then(|| rel.split('/').fold(hub.skills_dir(), |p, c| p.join(c)))
}

/// One `check` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Checked {
    /// The skill.
    pub name: String,
    /// Its identifier.
    pub identifier: String,
    /// `up_to_date`, `update_available`, `orphaned`, `invalid_install` or
    /// `unreachable`.
    pub state: String,
    /// Files differ from what was installed.
    pub locally_modified: bool,
    /// Detail (the unreachable reason).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

fn row(entry: &Entry, state: &str, locally_modified: bool, detail: Option<String>) -> Checked {
    Checked {
        name: entry.name.clone(),
        identifier: entry.source.clone(),
        state: state.to_owned(),
        locally_modified,
        detail,
    }
}

/// Checks every hub install (or `name`) against its source. Orphaned
/// directories and unsafe recorded paths are reported without a network
/// call.
///
/// # Errors
///
/// `skill.io`, `skill.not_hub`.
pub fn check(hub: &Hub, name: Option<&str>) -> Result<Vec<Checked>, Error> {
    let entries = match name {
        Some(name) => vec![entry(hub, name)?],
        None => entries(hub)?,
    };
    Ok(entries.iter().map(|e| check_one(hub, e)).collect())
}

fn check_one(hub: &Hub, entry: &Entry) -> Checked {
    let Some(dir) = install_dir(hub, entry) else {
        return row(entry, "invalid_install", false, None);
    };
    if !dir.join("SKILL.md").is_file() {
        return row(entry, "orphaned", false, None);
    }
    let modified = digest::dir(&dir) != entry.content_digest;
    let upstream = Identifier::parse(&entry.source)
        .and_then(|id| sources::fetch(hub, &id, None))
        .map(|bundle| bundle.digest());
    match upstream {
        Ok(digest) if digest == entry.content_digest => row(entry, "up_to_date", modified, None),
        Ok(_) => row(entry, "update_available", modified, None),
        Err(e) => row(entry, "unreachable", modified, Some(e.message().to_owned())),
    }
}

/// One `update` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Updated {
    /// The skill.
    pub name: String,
    /// `updated`, `skipped`, `up_to_date` or `failed`.
    pub action: String,
    /// Why (skipped, failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The reason a locally modified skill is not updated.
pub const LOCALLY_MODIFIED: &str = "locally modified; use --force";

/// Reinstalls hub skills whose upstream changed, through the full
/// pipeline; locally modified ones are skipped unless `force`.
///
/// # Errors
///
/// `skill.io`, `skill.not_hub`.
pub fn update(hub: &Hub, name: Option<&str>, force: bool) -> Result<Vec<Updated>, Error> {
    let checked = check(hub, name)?;
    Ok(checked
        .into_iter()
        .map(|c| {
            let done = |action: &str, detail: Option<String>| Updated {
                name: c.name.clone(),
                action: action.to_owned(),
                detail,
            };
            match c.state.as_str() {
                "update_available" if c.locally_modified && !force => {
                    done("skipped", Some(LOCALLY_MODIFIED.to_owned()))
                }
                "update_available" => match reinstall(hub, &c.identifier, Options::default()) {
                    Ok(()) => done("updated", None),
                    Err(e) => done("failed", Some(e.message().to_owned())),
                },
                "up_to_date" => done("up_to_date", None),
                _ => done("failed", Some(c.detail.unwrap_or(c.state))),
            }
        })
        .collect())
}

fn reinstall(hub: &Hub, identifier: &str, options: Options) -> Result<(), Error> {
    let id = Identifier::parse(identifier)?;
    install::install(
        hub,
        &id,
        &Options {
            force: true,
            ..options
        },
    )
    .map(drop)
}

/// One `audit` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Audited {
    /// The skill.
    pub name: String,
    /// The verdict (`dangerous` when the scan failed).
    pub verdict: String,
    /// The findings.
    pub summary: String,
    /// Quarantined by this audit.
    pub quarantined: bool,
}

/// Re-scans every hub install and quarantines `dangerous` ones (a scan
/// failure counts as dangerous).
///
/// # Errors
///
/// `skill.io`.
pub fn audit(hub: &Hub) -> Result<Vec<Audited>, Error> {
    let lock = LockFile::in_dir(&hub.skills_dir());
    let mut out = Vec::new();
    for entry in entries(hub)? {
        let Some(dir) = install_dir(hub, &entry).filter(|d| d.is_dir()) else {
            continue;
        };
        let files: Vec<(String, Vec<u8>)> = pmt_skills::layout::walk(&dir)
            .into_iter()
            .filter_map(|rel| std::fs::read(dir.join(&rel)).ok().map(|b| (rel, b)))
            .collect();
        let (verdict, summary) = match hub.scanner.scan(&files) {
            Ok(report) => (report.verdict, report.summary()),
            Err(e) => (Verdict::Dangerous, format!("security scan failed: {e}")),
        };
        let quarantine = verdict == Verdict::Dangerous;
        if quarantine {
            let why = format!("audit: {summary}");
            lock.modify(&entry.name, |e| e.quarantined = Some(why))?;
        }
        out.push(Audited {
            name: entry.name,
            verdict: verdict.to_string(),
            summary,
            quarantined: quarantine,
        });
    }
    Ok(out)
}

/// Removes hub skill `name` and its lock entry.
///
/// # Errors
///
/// `skill.not_hub`, `skill.io`.
pub fn uninstall(hub: &Hub, name: &str) -> Result<PathBuf, Error> {
    let entry = entry(hub, name)?;
    let dir = install_dir(hub, &entry).ok_or_else(|| {
        Error::new(
            &codes::SKILL_IO,
            format!("'{name}': the recorded install path is unsafe; remove the lock entry by hand"),
        )
    })?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| Error::io(&codes::SKILL_IO, &dir, &e))?;
    }
    LockFile::in_dir(&hub.skills_dir()).remove(name)?;
    Ok(dir)
}

/// Accepts the current files of a quarantined hub skill: re-scans and
/// re-records its digest.
///
/// # Errors
///
/// `skill.not_hub`, `skill.scan_blocked` for a `dangerous` scan,
/// `skill.io`.
pub fn accept(hub: &Hub, name: &str) -> Result<String, Error> {
    let entry = entry(hub, name)?;
    let dir = install_dir(hub, &entry)
        .filter(|d| d.is_dir())
        .ok_or_else(|| {
            Error::new(
                &codes::SKILL_NOT_FOUND,
                format!("'{name}' has no directory"),
            )
        })?;
    let files: Vec<(String, Vec<u8>)> = pmt_skills::layout::walk(&dir)
        .into_iter()
        .filter_map(|rel| std::fs::read(dir.join(&rel)).ok().map(|b| (rel, b)))
        .collect();
    let report = hub.scanner.scan(&files).map_err(|e| {
        Error::new(
            &codes::SKILL_SCAN_BLOCKED,
            format!("security scan failed: {e}"),
        )
    })?;
    if report.verdict == Verdict::Dangerous {
        return Err(Error::new(
            &codes::SKILL_SCAN_BLOCKED,
            format!(
                "'{name}' stays quarantined: security scan: {}",
                report.summary()
            ),
        ));
    }
    let digest = digest::dir(&dir);
    LockFile::in_dir(&hub.skills_dir()).modify(name, |e| {
        e.content_digest.clone_from(&digest);
        e.quarantined = None;
    })?;
    Ok(digest)
}

/// Restores hub skill `name` to its locked content from the source.
///
/// # Errors
///
/// `skill.not_hub`, `skill.lock_mismatch` when the source no longer
/// serves the locked content, and the pipeline's errors.
pub fn reset(hub: &Hub, name: &str) -> Result<(), Error> {
    let entry = entry(hub, name)?;
    reinstall(
        hub,
        &entry.source,
        Options {
            at: entry.resolved_ref.clone(),
            expect_digest: Some(entry.content_digest.clone()),
            ..Options::default()
        },
    )
}

/// One `install --locked` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Locked {
    /// The skill.
    pub name: String,
    /// `present`, `installed` or `failed`.
    pub action: String,
    /// Why it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Reproduces exactly the locked set: skills already matching their
/// digest stay, the rest are installed at their locked ref and must match
/// their locked digest.
///
/// # Errors
///
/// `skill.io`.
pub fn install_locked(hub: &Hub) -> Result<Vec<Locked>, Error> {
    let mut out = Vec::new();
    for entry in entries(hub)? {
        let present = install_dir(hub, &entry)
            .is_some_and(|d| d.is_dir() && digest::dir(&d) == entry.content_digest);
        let result = if present {
            Ok("present")
        } else {
            reset(hub, &entry.name).map(|()| "installed")
        };
        out.push(match result {
            Ok(action) => Locked {
                name: entry.name,
                action: action.to_owned(),
                detail: None,
            },
            Err(e) => Locked {
                name: entry.name,
                action: "failed".to_owned(),
                detail: Some(e.message().to_owned()),
            },
        });
    }
    Ok(out)
}
