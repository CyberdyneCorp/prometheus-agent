//! Writing entries (spec: experience-library, Entries are written on
//! adoption only; Entry contents; Answer-free and secret-free entries;
//! Retention cap). An entry is written only when a proposal that carries
//! a lesson is adopted; every adoption path goes through [`adopt`]. The
//! entry's tree, `EXPERIENCE.md` and the selector's reason pass the leak
//! globs, the verdict tripwire and the secret scanner first; a hit keeps
//! the adoption but writes no entry and records `lesson_rejected`.

use std::path::Path;

use pmt_config::harness::{HarnessDir, Tree};
use pmt_config::schema::{EvolveConfig, ExperienceConfig};
use pmt_core::error::Error;
use pmt_core::secret::Redactor;

use super::entry::{self, Document, EntryMeta, Status};
use super::library::{Library, NewEntry, Written};
use crate::engine::tripwire;
use crate::harness::Rules;
use crate::job::task::LeakGlobs;
use crate::proposal::{Adopted, Proposal, Proposals};

/// What the library needs from the profile's configuration.
#[derive(Debug, Clone)]
pub struct Settings {
    /// `evolve.leak_globs`.
    pub leak_globs: Vec<String>,
    /// The profile's secrets.
    pub redactor: Redactor,
    /// `experience.max_entries_per_domain`.
    pub cap: usize,
}

impl Settings {
    /// The settings of a profile.
    #[must_use]
    pub fn of(evolve: &EvolveConfig, experience: &ExperienceConfig, redactor: Redactor) -> Self {
        Self {
            leak_globs: evolve.leak_globs.clone(),
            redactor,
            cap: usize::try_from(experience.max_entries_per_domain).unwrap_or(usize::MAX),
        }
    }
}

/// What an adoption did to the library.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// The proposal carries no lesson: nothing to add.
    NoLesson,
    /// The entry was written.
    Written(Box<Written>),
    /// A screen fired: the reason recorded as `lesson_rejected`.
    Rejected(String),
    /// The library could not be written (logged; the adoption stands).
    Failed(String),
}

impl Outcome {
    /// The line the user is told.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::NoLesson => {
                "no transferable lesson; nothing will be added to the experience library".into()
            }
            Self::Written(w) => format!(
                "experience entry {} added to domain {}",
                w.meta.entry_id, w.meta.domain
            ),
            Self::Rejected(why) => {
                format!("lesson rejected ({why}); nothing was added to the experience library")
            }
            Self::Failed(why) => format!("experience entry not written: {why}"),
        }
    }
}

/// Adopts proposal `id` and records its lesson in the library.
///
/// # Errors
///
/// As [`Proposals::adopt`]; the library never fails the adoption.
pub fn adopt(
    profile_dir: &Path,
    id: &str,
    rebase: bool,
    rules: &Rules,
    settings: &Settings,
) -> Result<(Adopted, Outcome), Error> {
    let adopted = Proposals::of(profile_dir).adopt(profile_dir, id, rebase, rules)?;
    let outcome = record(profile_dir, settings, &adopted.proposal);
    Ok((adopted, outcome))
}

/// Writes the entry of adopted `proposal` (again, under its recorded id,
/// when a rebuild asks).
#[must_use]
pub fn record(profile_dir: &Path, settings: &Settings, proposal: &Proposal) -> Outcome {
    if proposal.lesson.is_none() {
        return Outcome::NoLesson;
    }
    match write(profile_dir, settings, proposal) {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::warn!(proposal = %proposal.id, error = %err, "experience entry not written");
            Outcome::Failed(err.to_string())
        }
    }
}

fn write(profile_dir: &Path, settings: &Settings, proposal: &Proposal) -> Result<Outcome, Error> {
    let Some(lesson) = &proposal.lesson else {
        return Ok(Outcome::NoLesson);
    };
    let version = proposal.adopted_version.clone().unwrap_or_default();
    let harness = HarnessDir::of(profile_dir);
    let tree = harness.tree(&version)?;
    let base = harness.tree(&proposal.base_version).ok();
    let created_at = jiff::Timestamp::now().to_string();
    let entry_id = proposal
        .entry_id
        .clone()
        .unwrap_or_else(|| entry::new_id(&proposal.title, &created_at));
    let inventory = entry::inventory(&tree, base.as_ref());
    let document = entry::render(&Document {
        title: &proposal.title,
        label: &proposal.label,
        job_id: &proposal.job_id,
        harness_version: &version,
        inventory: &inventory,
        reason: &proposal.reason,
        selected_by: &proposal.selected_by,
        edits: &proposal.edits,
        lesson,
    });
    let store = Proposals::of(profile_dir);
    if let Err((why, detail)) = screen(settings, &tree, &document, &proposal.reason) {
        tracing::warn!(proposal = %proposal.id, job = %proposal.job_id, "lesson rejected: {detail}");
        let mut rejected = store.get(&proposal.id)?;
        rejected.lesson_rejected = Some(why.clone());
        store.save(&rejected)?;
        return Ok(Outcome::Rejected(why));
    }
    let meta = EntryMeta {
        entry_id: entry_id.clone(),
        domain: proposal.domain.clone(),
        job_id: proposal.job_id.clone(),
        harness_version: version,
        mode: proposal.mode.clone(),
        k: proposal.k,
        confidence: lesson.confidence,
        created_at,
        pinned: false,
        status: Status::Active,
        times_shown: 0,
        title: proposal.title.clone(),
        cue: lesson.cue.clone(),
        lesson: lesson.lesson.clone(),
        inventory,
        proposal_id: proposal.id.clone(),
        imported_from: None,
    };
    let written = Library::of(profile_dir).write(
        &NewEntry {
            meta,
            document,
            tree,
        },
        settings.cap,
    )?;
    let mut saved = store.get(&proposal.id)?;
    saved.entry_id = Some(entry_id);
    saved.entry_dropped = false;
    saved.lesson_rejected = None;
    store.save(&saved)?;
    drop_entries(profile_dir, &written.evicted);
    Ok(Outcome::Written(Box::new(written)))
}

/// Marks the proposals of `entry_ids` so a rebuild leaves them out.
pub fn drop_entries(profile_dir: &Path, entry_ids: &[String]) {
    if entry_ids.is_empty() {
        return;
    }
    let store = Proposals::of(profile_dir);
    for mut p in store.list().unwrap_or_default() {
        if p.entry_id.as_ref().is_some_and(|e| entry_ids.contains(e)) {
            p.entry_dropped = true;
            if let Err(err) = store.save(&p) {
                tracing::warn!(proposal = %p.id, error = %err, "proposal not updated");
            }
        }
    }
}

/// Screens an entry: the screen that fired (`leak glob`, `verdict
/// tripwire`, `secret scanner`) and the detail for the log.
///
/// # Errors
///
/// The screen and the detail.
pub fn screen(
    settings: &Settings,
    tree: &Tree,
    document: &str,
    reason: &str,
) -> Result<(), (String, String)> {
    let leaks = LeakGlobs::new(&settings.leak_globs);
    if let Some(path) = tree.files.keys().find(|p| leaks.matches(p)) {
        return Err(("leak glob".into(), format!("{path} matches a leak glob")));
    }
    let texts = tree
        .files
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .chain([("EXPERIENCE.md", document), ("selector reason", reason)]);
    for (name, text) in texts.clone() {
        let named =
            tripwire::check_text(name, name).and_then(|()| tripwire::check_text(name, text));
        if let Err(hit) = named {
            return Err(("verdict tripwire".into(), hit));
        }
    }
    for (name, text) in texts {
        if settings.redactor.redact_all(text) != text {
            return Err(("secret scanner".into(), format!("{name} contains a secret")));
        }
    }
    Ok(())
}
