//! `memory status` and `/memory status` (spec: memory, Store enable flags
//! and tool availability): never claims built-in memory is active when a
//! store is disabled.

use std::fmt::Write as _;

use pmt_config::schema::MemoryConfig;
use pmt_core::error::Error;
use pmt_store::{MemoryStatus, MemoryTarget, MemoryTier, Store};
use serde::Serialize;

use crate::budget::{self, Usage};

/// One target's state.
#[derive(Debug, Clone, Serialize)]
pub struct TargetStatus {
    /// The target.
    pub target: MemoryTarget,
    /// Its enable flag.
    pub enabled: bool,
    /// Core usage.
    pub core: Usage,
    /// Core entries (not dormant).
    pub core_entries: usize,
    /// Archival entries (not dormant).
    pub archival_entries: usize,
    /// Dormant entries.
    pub dormant: usize,
    /// Blocked entries.
    pub blocked: usize,
}

/// The memory subsystem's state.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    /// `memory.provider`: `builtin` or the external provider's id.
    pub provider: String,
    /// Provider tools refused at registration (a name another owner
    /// holds), filled in by the caller that registers them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub collisions: Vec<String>,
    /// Whether any built-in store is enabled.
    pub enabled: bool,
    /// Per target.
    pub targets: Vec<TargetStatus>,
    /// Staged changes awaiting approval.
    pub pending: usize,
}

/// The current state.
///
/// # Errors
///
/// Store errors.
pub fn status(store: &Store, config: &MemoryConfig) -> Result<Status, Error> {
    let entries = store.memory_entries(None)?;
    let enabled = budget::enabled(config);
    let targets = MemoryTarget::ALL
        .iter()
        .map(|&target| {
            let of = |f: &dyn Fn(&pmt_store::MemoryEntry) -> bool| {
                entries
                    .iter()
                    .filter(|e| e.target == target && f(e))
                    .count()
            };
            TargetStatus {
                target,
                enabled: enabled.contains(&target),
                core: Usage::new(
                    budget::core_chars(&entries, target),
                    budget::limit(config, target),
                ),
                core_entries: of(&|e| budget::in_core(e, target)),
                archival_entries: of(&|e| {
                    e.tier == MemoryTier::Archival && e.status != MemoryStatus::Dormant
                }),
                dormant: of(&|e| e.status == MemoryStatus::Dormant),
                blocked: of(&|e| e.status == MemoryStatus::Blocked),
            }
        })
        .collect();
    Ok(Status {
        provider: config.provider.clone(),
        collisions: Vec::new(),
        enabled: !enabled.is_empty(),
        targets,
        pending: store.memory_pending()?.len(),
    })
}

/// The human form, worded with `text`.
#[must_use]
pub fn render(text: &dyn pmt_core::text::Catalog, status: &Status) -> String {
    let mut out = String::new();
    // `indent` is layout, not wording: it stays out of the catalog.
    let mut line = |indent: &str, key: &str, args: &[(&str, &dyn std::fmt::Display)]| {
        let _ = writeln!(out, "{indent}{}", text.text(key, args));
    };
    if status.enabled {
        line(
            "",
            "slash.memory.enabled",
            &[("provider", &status.provider)],
        );
    } else {
        line("", "slash.memory.disabled", &[]);
    }
    for t in &status.targets {
        if t.enabled {
            line(
                "  ",
                "slash.memory.target",
                &[
                    ("target", &t.target),
                    ("entries", &t.core_entries),
                    ("usage", &t.core.indicator()),
                    ("archival", &t.archival_entries),
                ],
            );
        } else {
            line(
                "  ",
                "slash.memory.target_off",
                &[("target", &t.target), ("flag", &budget::flag(t.target))],
            );
        }
        if t.blocked > 0 {
            line("    ", "slash.memory.blocked", &[("count", &t.blocked)]);
        }
    }
    for collision in &status.collisions {
        line("  ", "slash.memory.collision", &[("tool", collision)]);
    }
    if status.pending > 0 {
        line(
            "  ",
            "slash.memory.pending_count",
            &[("count", &status.pending)],
        );
    }
    out.trim_end().to_owned()
}
