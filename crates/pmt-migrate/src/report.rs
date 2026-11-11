//! The mapping report (spec: hermes-migration, Mapping report): every
//! Hermes config key, `.env` entry, credential file, skill, cron job, MCP
//! server and plugin gets exactly one [`Disposition`] with its Prometheus
//! target and a reason. Entries name secrets but never carry their values
//! or message content: [`Entry::value`] is only set for non-secret config
//! values the user may want to see (an unsupported skin name).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;

use crate::component::Component;

/// What happened to one Hermes item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// Carried over unchanged.
    Mapped,
    /// Carried over with a change (renamed unit, rewritten schedule...).
    Transformed,
    /// Equal to the Hermes default: Prometheus' own default applies.
    DefaultOmitted,
    /// Stored in the secrets store.
    Secret,
    /// No Prometheus equivalent; nothing written.
    Unsupported,
    /// A value whose meaning is unclear; the restrictive reading was taken.
    Ambiguous,
    /// The destination already holds another value; it was kept.
    Conflict,
    /// Deliberately not imported.
    Skipped,
    /// The item could not be imported.
    Failed,
}

impl Disposition {
    /// The wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mapped => "mapped",
            Self::Transformed => "transformed",
            Self::DefaultOmitted => "default_omitted",
            Self::Secret => "secret",
            Self::Unsupported => "unsupported",
            Self::Ambiguous => "ambiguous",
            Self::Conflict => "conflict",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }
}

/// One item's disposition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// The component that owns the item.
    pub component: Component,
    /// The Hermes item: a dotted config key, `.env:<NAME>`, a file, a
    /// skill, job, server or plugin name.
    pub item: String,
    /// What happened.
    pub disposition: Disposition,
    /// Where it went in Prometheus (a config key, a secret name, a path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Why.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// The (non-secret) Hermes value, when it helps to show it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

impl Entry {
    /// An entry without target, reason or value.
    #[must_use]
    pub fn new(component: Component, item: impl Into<String>, disposition: Disposition) -> Self {
        Self {
            component,
            item: item.into(),
            disposition,
            target: None,
            reason: String::new(),
            value: None,
        }
    }

    /// Sets the target.
    #[must_use]
    pub fn to(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    /// Sets the reason.
    #[must_use]
    pub fn because(mut self, reason: impl Into<String>) -> Self {
        self.reason = reason.into();
        self
    }

    /// Shows the (non-secret) value.
    #[must_use]
    pub fn showing(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }
}

/// How a component ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Dry run: computed, nothing written.
    Planned,
    /// Written.
    Committed,
    /// Nothing written; see the error.
    Failed,
    /// Stopped by Ctrl+C before it committed (sessions: after its last
    /// committed batch); a re-run continues it.
    Interrupted,
}

/// One component of one profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComponentReport {
    /// The component.
    pub component: Component,
    /// How it ended.
    pub status: Status,
    /// Why it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Items per disposition.
    pub counts: BTreeMap<Disposition, usize>,
}

/// One Hermes profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileReport {
    /// The Hermes profile (`default` for the root).
    pub hermes_profile: String,
    /// The Prometheus profile it went to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The Hermes profile directory.
    pub source: String,
    /// Why the whole profile failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The error code of `error` (`migrate.destination`: the profile
    /// cannot be written).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Each selected component.
    pub components: Vec<ComponentReport>,
    /// Every item.
    pub entries: Vec<Entry>,
    /// Notices for the summary (WARN lines).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl ProfileReport {
    /// Whether any component (or the profile itself) failed.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.error.is_some() || self.components.iter().any(|c| c.status == Status::Failed)
    }
}

/// A profile directory that was not migrated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedProfile {
    /// The Hermes profile name.
    pub name: String,
    /// `tombstoned`, `no Hermes files`...
    pub reason: String,
}

/// The whole run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    /// `prometheus.migration_report.v1`.
    pub schema: &'static str,
    /// This run's id.
    pub run_id: String,
    /// The Hermes home.
    pub source: String,
    /// Nothing was written.
    pub dry_run: bool,
    /// Each migrated (or planned) profile.
    pub profiles: Vec<ProfileReport>,
    /// Profile directories left out.
    pub skipped_profiles: Vec<SkippedProfile>,
    /// The run was interrupted (Ctrl+C); re-running resumes it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub interrupted: bool,
    /// Run-wide notices.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// The report's JSON shape name.
pub const SCHEMA: &str = "prometheus.migration_report.v1";

impl Report {
    /// Whether any profile or component failed.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.profiles.iter().any(ProfileReport::failed)
    }

    /// The exit status (spec: hermes-migration, Migration exit codes and
    /// confirmation): 130 interrupted, 73 when a destination profile
    /// cannot be written, 1 when any component failed, else 0
    /// (unsupported and conflict entries do not fail a run).
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        let unwritable = self
            .profiles
            .iter()
            .any(|p| p.code.as_deref() == Some(pmt_core::error::codes::MIGRATE_DESTINATION.code));
        if self.interrupted {
            pmt_core::exit::INTERRUPTED
        } else if unwritable {
            pmt_core::exit::CANNOT_CREATE_OUTPUT
        } else if self.failed() {
            pmt_core::exit::FAILURE
        } else {
            pmt_core::exit::SUCCESS
        }
    }

    /// The human summary: per profile, the counts per component and every
    /// entry that needs attention (unsupported, ambiguous, conflict,
    /// failed).
    #[must_use]
    pub fn summary(&self) -> String {
        let mut out = String::new();
        let verb = match (self.dry_run, self.interrupted) {
            (true, _) => "Plan",
            (false, true) => "Interrupted migration",
            (false, false) => "Migrated",
        };
        let _ = writeln!(out, "{verb} from {}", self.source);
        for warning in &self.warnings {
            let _ = writeln!(out, "WARN {warning}");
        }
        for p in &self.profiles {
            summarize_profile(&mut out, p);
        }
        for s in &self.skipped_profiles {
            let _ = writeln!(out, "profile {}: skipped ({})", s.name, s.reason);
        }
        out
    }
}

fn summarize_profile(out: &mut String, p: &ProfileReport) {
    let target = p.target.as_deref().unwrap_or("-");
    let _ = writeln!(out, "profile {} -> {target}", p.hermes_profile);
    if let Some(error) = &p.error {
        let _ = writeln!(out, "  failed: {error}");
    }
    for warning in &p.warnings {
        let _ = writeln!(out, "  WARN {warning}");
    }
    for c in &p.components {
        let counts: Vec<String> = c
            .counts
            .iter()
            .map(|(d, n)| format!("{n} {}", d.as_str()))
            .collect();
        let status = match (c.status, &c.error) {
            (Status::Failed, Some(e)) => format!("failed: {e}"),
            (Status::Interrupted, _) => "interrupted; rerun to continue".to_owned(),
            _ if counts.is_empty() => "nothing to import".to_owned(),
            _ => counts.join(", "),
        };
        let _ = writeln!(out, "  {:<9} {status}", c.component.as_str());
    }
    let notable = p.entries.iter().filter(|e| {
        matches!(
            e.disposition,
            Disposition::Unsupported
                | Disposition::Ambiguous
                | Disposition::Conflict
                | Disposition::Failed
        )
    });
    for e in notable {
        let mut line = format!("    {} {}: {}", e.disposition.as_str(), e.item, e.reason);
        if let Some(value) = &e.value {
            let _ = write!(line, " (value {value})");
        }
        let _ = writeln!(out, "{line}");
    }
}

impl Report {
    /// The per-item plan: one line per entry of every profile, what it
    /// becomes and why.
    #[must_use]
    pub fn items(&self) -> String {
        let mut out = String::new();
        for p in &self.profiles {
            let target = p.target.as_deref().unwrap_or("-");
            let _ = writeln!(out, "{} -> profile {target}", p.source);
            for e in &p.entries {
                let mut line = format!(
                    "  {:<9} {:<15} {}",
                    e.component.as_str(),
                    e.disposition.as_str(),
                    e.item
                );
                if let Some(t) = &e.target {
                    let _ = write!(line, " -> {t}");
                }
                if !e.reason.is_empty() {
                    let _ = write!(line, " ({})", e.reason);
                }
                let _ = writeln!(out, "{line}");
            }
        }
        out
    }
}

/// Counts `entries` of `component` per disposition.
#[must_use]
pub fn counts(entries: &[Entry], component: Component) -> BTreeMap<Disposition, usize> {
    let mut counts = BTreeMap::new();
    for e in entries.iter().filter(|e| e.component == component) {
        *counts.entry(e.disposition).or_insert(0) += 1;
    }
    counts
}
