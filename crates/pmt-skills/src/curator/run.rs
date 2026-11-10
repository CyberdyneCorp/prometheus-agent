//! One curator pass: under the profile's curator lock, plan, back the
//! tree up (real passes), apply the transitions and merges, record the
//! last-run time, and write the run report. A dry run changes neither
//! the skills tree nor `skill_usage`.

use std::fs::File;

use pmt_core::error::{Error, codes};
use pmt_core::file_lock;
use pmt_store::skill_usage::SkillState;

use super::consolidate::{self, Consolidator};
use super::report::{self, RunReport};
use super::{Scope, backup, managed, moves, plan};
use crate::ledger::{Actor, Evidence, Ledger};
use crate::scan::Scanner;

/// How to run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// Plan and report only.
    pub dry_run: bool,
    /// Also merge overlapping skills (`--consolidate` or
    /// `curator.consolidate`).
    pub consolidate: bool,
    /// An automatic pass.
    pub automatic: bool,
}

/// The held curator lock of a profile.
pub struct PassLock {
    _file: File,
}

/// Takes the profile's curator lock: at most one pass at a time.
///
/// # Errors
///
/// `curator.busy` when another pass holds it, `skill.io`.
pub fn lock(scope: &Scope<'_>) -> Result<PassLock, Error> {
    let dir = scope.skills_dir().join(".locks");
    pmt_core::private_fs::create_dir(&dir).map_err(|e| Error::io(&codes::SKILL_IO, &dir, &e))?;
    let path = dir.join("curator.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| Error::io(&codes::SKILL_IO, &path, &e))?;
    file_lock::try_lock(&file).map_err(|_| {
        Error::new(
            &codes::CURATOR_BUSY,
            "another curator pass of this profile is running",
        )
    })?;
    Ok(PassLock { _file: file })
}

/// Runs one pass.
///
/// # Errors
///
/// `curator.busy`, `skill.io` (the backup failed: nothing was changed),
/// store errors.
pub fn run(
    scope: &Scope<'_>,
    options: Options,
    consolidator: Option<&dyn Consolidator>,
    scanner: &dyn Scanner,
) -> Result<RunReport, Error> {
    let _lock = lock(scope)?;
    let planned = plan(scope);
    let mut report = RunReport {
        started_at: scope.now.to_string(),
        dry_run: options.dry_run,
        automatic: options.automatic,
        transitions: planned.transitions.clone(),
        skipped: planned.skipped,
        ..RunReport::default()
    };
    let stamp = scope.now.strftime("%Y%m%dT%H%M%SZ").to_string();
    if !options.dry_run {
        let keep = usize::try_from(scope.config.curator.backup_keep).unwrap_or(2);
        report.backup = Some(backup::write(&scope.skills_dir(), keep, scope.now)?);
        apply(scope, &mut report);
        if let Some(consolidator) = consolidator.filter(|_| options.consolidate) {
            merge(scope, consolidator, scanner, &mut report);
        }
        scope.store.set_curator_last_run(scope.now)?;
    }
    report.report_dir = Some(report::write(scope.profile_dir, &report, &stamp)?);
    Ok(report)
}

fn apply(scope: &Scope<'_>, report: &mut RunReport) {
    let ledger = Ledger::new(
        scope.store,
        &scope.skills_dir(),
        scope.config.skills.ledger_max_bytes,
    );
    let (managed, _) = managed(scope);
    for t in &report.transitions {
        let result = match t.to.as_str() {
            "archived" => managed
                .iter()
                .find(|m| m.skill.name == t.name)
                .map_or(Ok(()), |m| {
                    moves::archive(
                        &ledger,
                        scope.store,
                        &t.name,
                        &m.rel,
                        Actor::Curator,
                        &Evidence::default(),
                    )
                    .map(|to| {
                        report.archived.insert(t.name.clone(), to);
                    })
                }),
            "stale" => scope.store.set_skill_state(&t.name, SkillState::Stale),
            _ => scope.store.set_skill_state(&t.name, SkillState::Active),
        };
        if let Err(e) = result {
            report.errors.push(format!("{}: {}", t.name, e.message()));
        }
    }
}

fn merge(
    scope: &Scope<'_>,
    consolidator: &dyn Consolidator,
    scanner: &dyn Scanner,
    report: &mut RunReport,
) {
    // The catalog of the plan is stale after archive moves: only skills
    // still in place are candidates.
    let (managed, _) = managed(scope);
    let remaining: Vec<_> = managed
        .into_iter()
        .filter(|m| !report.archived.contains_key(&m.skill.name))
        .collect();
    if remaining.len() < 2 {
        return;
    }
    let candidates = consolidate::candidates(&remaining);
    let merges = match consolidator.propose(&candidates) {
        Ok(merges) => merges,
        Err(e) => {
            report
                .errors
                .push(format!("consolidation review: {}", e.message()));
            return;
        }
    };
    let ledger = Ledger::new(
        scope.store,
        &scope.skills_dir(),
        scope.config.skills.ledger_max_bytes,
    );
    let applied = consolidate::apply(
        &ledger,
        scope.store,
        &remaining,
        &merges,
        scanner,
        &mut report.errors,
    );
    for a in &applied {
        for old in &a.absorbed {
            report.rename_map.insert(old.clone(), a.into.clone());
        }
    }
    report.consolidated = applied;
}
