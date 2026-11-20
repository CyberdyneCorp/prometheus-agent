//! Library maintenance (spec: experience-library, Library CLI; Rollback
//! demotes entries; Restore on resume and repair; Lessons offered to
//! later sessions): what the CLI, the RPC methods and the session factory
//! do to entries beyond writing them.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

use pmt_config::harness::{HarnessDir, Source};
use pmt_core::error::Error;

use super::entry::{EntryMeta, Status};
use super::lesson::Confidence;
use super::library::Library;
use super::record::{self, Outcome, Settings};
use crate::proposal::{Proposals, Status as ProposalStatus};

/// The most characters of the session lessons layer.
pub const LESSONS_CAP_CHARS: usize = 2000;

/// Removes entry `id`; its proposal is marked so a rebuild leaves it out.
///
/// # Errors
///
/// `experience.entry_not_found`, `experience.io`.
pub fn forget(profile_dir: &Path, id: &str) -> Result<EntryMeta, Error> {
    let meta = Library::of(profile_dir).forget(id)?;
    record::drop_entries(profile_dir, std::slice::from_ref(&meta.entry_id));
    Ok(meta)
}

/// Turns entry `id` off: hidden from proposers and sessions until
/// restored (spec: web-dashboard, Experience library view).
///
/// # Errors
///
/// As [`Library::set_status`].
pub fn disable(profile_dir: &Path, id: &str) -> Result<EntryMeta, Error> {
    Library::of(profile_dir).set_status(id, Status::Disabled)
}

/// Sets entry `id` back to `active`.
///
/// # Errors
///
/// As [`Library::set_status`].
pub fn restore(profile_dir: &Path, id: &str) -> Result<EntryMeta, Error> {
    Library::of(profile_dir).set_status(id, Status::Active)
}

/// Pins or unpins entry `id`.
///
/// # Errors
///
/// As [`Library::update`].
pub fn pin(profile_dir: &Path, id: &str, pinned: bool) -> Result<EntryMeta, Error> {
    Library::of(profile_dir).update(id, |m| m.pinned = pinned)
}

/// The versions whose content the active harness descends from: the
/// parent chain, where a rollback version continues from the version it
/// restored (the newest earlier version with the same fingerprint), not
/// from the version it replaced.
///
/// # Errors
///
/// `harness.io`.
pub fn lineage(profile_dir: &Path) -> Result<BTreeSet<String>, Error> {
    let harness = HarnessDir::of(profile_dir);
    let versions = harness.versions()?;
    let mut seen = BTreeSet::new();
    let mut current = Some(harness.active_id()?);
    while let Some(id) = current.take() {
        if !seen.insert(id.clone()) {
            break;
        }
        let Some(info) = versions.iter().find(|v| v.id == id) else {
            break;
        };
        current = if info.source == Source::Rollback {
            versions
                .iter()
                .filter(|v| v.fingerprint == info.fingerprint && v.sequence() < info.sequence())
                .max_by_key(|v| v.sequence())
                .map(|v| v.id.clone())
                .or_else(|| info.parent.clone())
        } else {
            info.parent.clone()
        };
    }
    Ok(seen)
}

/// After a rollback: every active, unpinned entry whose harness version
/// the active harness no longer descends from becomes `rolled-back`; the
/// ids demoted. Imported entries name another profile's version, which
/// was never in this lineage, so a rollback leaves them alone.
///
/// # Errors
///
/// `harness.io`, `experience.io`.
pub fn demote_after_rollback(profile_dir: &Path) -> Result<Vec<String>, Error> {
    let lineage = lineage(profile_dir)?;
    let library = Library::of(profile_dir);
    let mut demoted = Vec::new();
    for meta in library.all() {
        let foreign = meta.imported_from.is_some();
        if meta.visible() && !meta.pinned && !foreign && !lineage.contains(&meta.harness_version) {
            library.set_status(&meta.entry_id, Status::RolledBack)?;
            demoted.push(meta.entry_id);
        }
    }
    Ok(demoted)
}

/// Re-creates the missing entries of adopted proposals with a lesson
/// (in `domain`, or every domain), from the proposal record and its
/// harness version, without any model call; the ids restored.
///
/// # Errors
///
/// `harness.io`.
pub fn rebuild(
    profile_dir: &Path,
    settings: &Settings,
    domain: Option<&str>,
) -> Result<Vec<String>, Error> {
    let library = Library::of(profile_dir);
    let present: BTreeSet<String> = library.all().into_iter().map(|e| e.entry_id).collect();
    let mut restored = Vec::new();
    let candidates = Proposals::of(profile_dir).list()?.into_iter().filter(|p| {
        p.status == ProposalStatus::Adopted
            && p.lesson.is_some()
            && p.lesson_rejected.is_none()
            && !p.entry_dropped
            && domain.is_none_or(|d| p.domain == d)
            && !p.entry_id.as_ref().is_some_and(|e| present.contains(e))
    });
    for proposal in candidates {
        match record::record(profile_dir, settings, &proposal) {
            Outcome::Written(w) => restored.push(w.meta.entry_id),
            other => {
                tracing::warn!(proposal = %proposal.id, "entry not restored: {}", other.message());
            }
        }
    }
    if let Some(domain) = domain {
        library.ensure(domain)?;
    }
    Ok(restored)
}

/// The session lessons layer of `domain`: at most `max` visible entries
/// of confidence high or medium (pinned first, then newest) as
/// `cue → lesson` lines under `## Lessons from earlier work in <domain>`,
/// capped at 2,000 characters; the text and the entries it lists.
#[must_use]
pub fn session_lessons(
    profile_dir: &Path,
    domain: &str,
    max: usize,
) -> Option<(String, Vec<String>)> {
    let picked: Vec<EntryMeta> = Library::of(profile_dir)
        .visible(domain)
        .into_iter()
        .filter(|e| e.confidence >= Confidence::Medium && !e.lesson.is_empty())
        .take(max)
        .collect();
    let mut text = format!("## Lessons from earlier work in {domain}\n");
    let mut ids = Vec::new();
    for entry in &picked {
        let mut line = String::new();
        let _ = writeln!(line, "- {} → {}", entry.cue, entry.lesson);
        if text.chars().count() + line.chars().count() > LESSONS_CAP_CHARS {
            break;
        }
        text.push_str(&line);
        ids.push(entry.entry_id.clone());
    }
    (!ids.is_empty()).then(|| (text.trim_end().to_owned(), ids))
}

/// Counts one more showing of each of `ids`.
pub fn shown(profile_dir: &Path, ids: &[String]) {
    let library = Library::of(profile_dir);
    for id in ids {
        if let Err(err) = library.update(id, |m| m.times_shown += 1) {
            tracing::warn!(entry = %id, error = %err, "times_shown not counted");
        }
    }
}
