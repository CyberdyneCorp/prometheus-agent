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
    /// `builtin` (external providers arrive in M2).
    pub provider: &'static str,
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
        provider: "builtin",
        enabled: !enabled.is_empty(),
        targets,
        pending: store.memory_pending()?.len(),
    })
}

/// The human form.
#[must_use]
pub fn render(status: &Status) -> String {
    let mut out = String::new();
    if status.enabled {
        out.push_str("Built-in memory: enabled (provider: builtin)\n");
    } else {
        out.push_str("Built-in memory is disabled (memory.memory_enabled = false, memory.user_profile_enabled = false); the memory tool is not offered.\n");
    }
    for t in &status.targets {
        if t.enabled {
            let _ = writeln!(
                out,
                "  {}: {} core entries {}, {} archival",
                t.target,
                t.core_entries,
                t.core.indicator(),
                t.archival_entries
            );
        } else {
            let _ = writeln!(
                out,
                "  {}: disabled ({} = false)",
                t.target,
                budget::flag(t.target)
            );
        }
        if t.blocked > 0 {
            let _ = writeln!(
                out,
                "    {} blocked by the threat scan (prometheus memory list --all)",
                t.blocked
            );
        }
    }
    if status.pending > 0 {
        let _ = writeln!(
            out,
            "  {} pending change(s): prometheus memory pending",
            status.pending
        );
    }
    out.trim_end().to_owned()
}
