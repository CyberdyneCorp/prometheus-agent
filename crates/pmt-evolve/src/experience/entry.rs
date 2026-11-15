//! One library entry (spec: experience-library, Library layout; Entry
//! contents): `entries/<entry_id>/` with `EXPERIENCE.md`, `entry.toml`
//! and `harness/` (the whole adopted tree, never a summary).

use std::fmt::Write as _;

use pmt_config::harness::{ALWAYS_ON, Census, Tree};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::lesson::{Confidence, Edit, Lesson};
use crate::harness::diff;

/// The longest task slug in an entry id.
pub const SLUG_LEN: usize = 40;

/// An entry's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// Shown to proposers and sessions.
    #[default]
    Active,
    /// Its harness was rolled back: hidden until restored.
    RolledBack,
}

impl Status {
    /// The spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::RolledBack => "rolled-back",
        }
    }
}

/// `entry.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryMeta {
    /// `<yyyymmdd>-<task_slug>-<6 hex>`.
    pub entry_id: String,
    /// Its domain.
    pub domain: String,
    /// The job whose proposal was adopted.
    pub job_id: String,
    /// The harness version adoption wrote.
    pub harness_version: String,
    /// `parallel` or `sequential`.
    pub mode: String,
    /// The job's budget k.
    pub k: u8,
    /// The lesson's confidence.
    pub confidence: Confidence,
    /// RFC 3339.
    pub created_at: String,
    /// Never evicted, never demoted by a rollback.
    pub pinned: bool,
    /// `active` or `rolled-back`.
    pub status: Status,
    /// Sessions the lesson was offered to.
    pub times_shown: u64,
    /// The task title.
    #[serde(default)]
    pub title: String,
    /// The lesson's cue (index, listings).
    #[serde(default)]
    pub cue: String,
    /// The lesson (session layer).
    #[serde(default)]
    pub lesson: String,
    /// The components line.
    #[serde(default)]
    pub inventory: String,
    /// The proposal it came from.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub proposal_id: String,
    /// The archive it was imported from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_from: Option<String>,
}

impl EntryMeta {
    /// Shown to proposers and sessions.
    #[must_use]
    pub fn visible(&self) -> bool {
        self.status == Status::Active
    }
}

/// The task title: the first non-empty line of `task.md`, without a
/// Markdown heading marker.
#[must_use]
pub fn title_of(statement: &str) -> String {
    let line = statement
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .unwrap_or("untitled task");
    line.chars().take(120).collect()
}

/// `title` lower-cased, runs outside `[a-z0-9]` replaced by `-`, cut to
/// 40 characters.
#[must_use]
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let cut: String = out.trim_matches('-').chars().take(SLUG_LEN).collect();
    let cut = cut.trim_end_matches('-');
    if cut.is_empty() {
        "task".to_owned()
    } else {
        cut.to_owned()
    }
}

/// A fresh entry id for `title` created at `created_at` (RFC 3339).
#[must_use]
pub fn new_id(title: &str, created_at: &str) -> String {
    let date: String = created_at
        .chars()
        .take(10)
        .filter(char::is_ascii_digit)
        .collect();
    let seed = format!("{created_at}|{title}|{}", ulid::Ulid::generate());
    let digest = Sha256::digest(seed.as_bytes());
    let hex: String = digest[..3].iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    });
    format!("{date}-{}-{hex}", slug(title))
}

/// The components line of `tree` adopted over `base`.
#[must_use]
pub fn inventory(tree: &Tree, base: Option<&Tree>) -> String {
    if let Some(base) = base {
        let summary = diff::summary(base, tree);
        let changed: Vec<&String> = summary
            .added
            .iter()
            .chain(&summary.modified)
            .chain(&summary.removed)
            .collect();
        if !changed.is_empty() && changed.iter().all(|p| ALWAYS_ON.contains(&p.as_str())) {
            return "always-on edits only".to_owned();
        }
    }
    let c = Census::of(tree);
    format!(
        "{} skills, {} scripts, {} agents, {} hooks; {} always-on chars",
        c.skills, c.scripts, c.agents, c.hooks, c.always_on_chars
    )
}

/// What `EXPERIENCE.md` is written from.
#[derive(Debug, Clone)]
pub struct Document<'a> {
    /// The task title.
    pub title: &'a str,
    /// The chosen final's label.
    pub label: &'a str,
    /// The job.
    pub job_id: &'a str,
    /// The adopted version.
    pub harness_version: &'a str,
    /// The components line.
    pub inventory: &'a str,
    /// The selector's reason.
    pub reason: &'a str,
    /// `model`, `user` or `single`.
    pub selected_by: &'a str,
    /// The `<edits>` rows.
    pub edits: &'a [Edit],
    /// The lesson.
    pub lesson: &'a Lesson,
}

/// `EXPERIENCE.md`.
#[must_use]
pub fn render(doc: &Document<'_>) -> String {
    let mut text = format!(
        "# {}\n\n- adopted: {} (job {}, harness {})\n- components: {}\n\n## Why this was chosen\n\n",
        doc.title, doc.label, doc.job_id, doc.harness_version, doc.inventory
    );
    let why = match doc.selected_by {
        "user" => "selected by user",
        "single" if doc.reason.trim().is_empty() => "the only final candidate (k = 1)",
        _ if doc.reason.trim().is_empty() => "(no reason given)",
        _ => doc.reason.trim(),
    };
    let _ = writeln!(text, "{why}\n\n## What each edit did\n");
    if doc.edits.is_empty() {
        text.push_str("(no edit rows)\n");
    }
    for e in doc.edits {
        let _ = writeln!(
            text,
            "- {} ({}): {} Observed: {} Verdict: {}",
            e.component, e.candidates, e.change, e.observed, e.verdict
        );
    }
    let l = doc.lesson;
    let _ = write!(
        text,
        "\n## Transferable lesson\n\n- cue: {}\n- lesson: {}\n- evidence: {}\n- confidence: {}\n",
        l.cue,
        l.lesson,
        l.evidence,
        l.confidence_line()
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_ids_carry_the_date_the_slug_and_six_hex() {
        let id = new_id("Reconcile Q3 ledger", "2026-10-29T10:00:00Z");
        let re = regex::Regex::new(r"^20261029-reconcile-q3-ledger-[0-9a-f]{6}$").unwrap();
        assert!(re.is_match(&id), "{id}");
        assert_eq!(slug(&"A b ".repeat(30)).len(), SLUG_LEN - 1);
        assert_eq!(
            title_of("\n# Reconcile Q3 ledger\nmore"),
            "Reconcile Q3 ledger"
        );
    }

    /// Scenario: Inventory wording.
    #[test]
    fn a_guardrails_only_change_reads_always_on_edits_only() {
        let base = Tree::builtin();
        let mut tree = base.clone();
        tree.files
            .insert("GUARDRAILS.md".into(), "Sum refunds too.".into());
        assert_eq!(inventory(&tree, Some(&base)), "always-on edits only");
        tree.files
            .insert("scripts/sum.py".into(), "print(1)\n".into());
        let line = inventory(&tree, Some(&base));
        assert!(
            line.starts_with("0 skills, 1 scripts, 0 agents, 0 hooks; "),
            "{line}"
        );
    }
}
