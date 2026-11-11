//! The record of every curator run: `<profile>/logs/curator/<UTC
//! timestamp>/run.json` and `REPORT.md`, with the rename map `old → new`
//! of consolidations.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use serde::Serialize;

use super::consolidate::Applied;
use super::{Skipped, Transition};

/// What one run did (or, dry, would do).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RunReport {
    /// When it started (RFC 3339).
    pub started_at: String,
    /// Nothing was changed.
    pub dry_run: bool,
    /// An automatic pass (not `curator run`).
    pub automatic: bool,
    /// The transitions (planned, when dry).
    pub transitions: Vec<Transition>,
    /// Managed skills left alone.
    pub skipped: Vec<Skipped>,
    /// Archive paths of archived skills, by name.
    pub archived: BTreeMap<String, String>,
    /// Merges made.
    pub consolidated: Vec<Applied>,
    /// `old → new` of every absorbed skill.
    pub rename_map: BTreeMap<String, String>,
    /// The backup written before the changes.
    pub backup: Option<PathBuf>,
    /// What went wrong without stopping the run.
    pub errors: Vec<String>,
    /// Where `run.json` and `REPORT.md` are.
    pub report_dir: Option<PathBuf>,
}

impl RunReport {
    /// The one-paragraph summary a chat shows.
    #[must_use]
    pub fn summary(&self) -> String {
        let count = |to: &str| self.transitions.iter().filter(|t| t.to == to).count();
        let verb = if self.dry_run { "would mark" } else { "marked" };
        let mut text = format!(
            "Curator {}: {verb} {} stale, {} active again, {} archived",
            if self.dry_run { "dry run" } else { "pass" },
            count("stale"),
            count("active"),
            count("archived"),
        );
        if !self.consolidated.is_empty() {
            let _ = write!(text, ", {} merge(s)", self.consolidated.len());
        }
        text.push('.');
        if !self.rename_map.is_empty() {
            let renames: Vec<String> = self
                .rename_map
                .iter()
                .map(|(old, new)| format!("{old} → {new}"))
                .collect();
            let _ = write!(text, " Renamed: {}.", renames.join(", "));
        }
        if let Some(dir) = &self.report_dir {
            let _ = write!(text, " Report: {}", dir.join("REPORT.md").display());
        }
        text
    }

    /// The `REPORT.md` text.
    #[must_use]
    pub fn markdown(&self) -> String {
        let mut text = format!(
            "# Curator {}\n\nStarted {}{}.\n\n",
            if self.dry_run { "dry run" } else { "run" },
            self.started_at,
            if self.automatic { " (automatic)" } else { "" }
        );
        if let Some(backup) = &self.backup {
            let _ = writeln!(text, "Backup: `{}`\n", backup.display());
        }
        text.push_str("## Transitions\n\n");
        if self.transitions.is_empty() {
            text.push_str("None.\n");
        }
        for t in &self.transitions {
            let _ = writeln!(
                text,
                "- {}: {} → {} ({} days idle)",
                t.name, t.from, t.to, t.idle_days
            );
        }
        text.push_str("\n## Rename map\n\n");
        if self.rename_map.is_empty() {
            text.push_str("None.\n");
        }
        for (old, new) in &self.rename_map {
            let _ = writeln!(text, "- {old} → {new}");
        }
        if !self.skipped.is_empty() {
            text.push_str("\n## Left alone\n\n");
            for s in &self.skipped {
                let _ = writeln!(text, "- {}: {}", s.name, s.reason);
            }
        }
        if !self.errors.is_empty() {
            text.push_str("\n## Problems\n\n");
            for e in &self.errors {
                let _ = writeln!(text, "- {e}");
            }
        }
        text
    }
}

/// Writes `run.json` and `REPORT.md` under
/// `<profile>/logs/curator/<stamp>/`; the directory.
///
/// # Errors
///
/// `skill.io`.
pub fn write(profile_dir: &Path, report: &RunReport, stamp: &str) -> Result<PathBuf, Error> {
    let mut dir = profile_dir.join("logs").join("curator").join(stamp);
    let mut n = 1;
    while dir.exists() {
        dir = profile_dir
            .join("logs")
            .join("curator")
            .join(format!("{stamp}-{n}"));
        n += 1;
    }
    private_fs::create_dir(&dir).map_err(|e| Error::io(&codes::SKILL_IO, &dir, &e))?;
    let mut with_dir = report.clone();
    with_dir.report_dir = Some(dir.clone());
    let json = serde_json::to_string_pretty(&with_dir).unwrap_or_default();
    for (name, text) in [("run.json", json), ("REPORT.md", with_dir.markdown())] {
        let path = dir.join(name);
        private_fs::write_atomic(&path, text.as_bytes())
            .map_err(|e| Error::io(&codes::SKILL_IO, &path, &e))?;
    }
    Ok(dir)
}
