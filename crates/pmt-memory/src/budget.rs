//! Targets, their enable flags and the core tier's character budget (spec:
//! memory, Core tier character budgets; Store enable flags and tool
//! availability).

use pmt_config::schema::MemoryConfig;
use pmt_store::{MemoryEntry, MemoryStatus, MemoryTarget, MemoryTier};
use serde::Serialize;

/// Separator of rendered entries, in the prompt and in the budget.
pub const SEPARATOR: &str = "\n§\n";

/// The targets `config` enables, in schema order.
#[must_use]
pub fn enabled(config: &MemoryConfig) -> Vec<MemoryTarget> {
    let mut targets = Vec::new();
    if config.memory_enabled {
        targets.push(MemoryTarget::Memory);
    }
    if config.user_profile_enabled {
        targets.push(MemoryTarget::User);
    }
    targets
}

/// The core character budget of `target`.
#[must_use]
pub fn limit(config: &MemoryConfig, target: MemoryTarget) -> usize {
    let limit = match target {
        MemoryTarget::Memory => config.memory_char_limit,
        MemoryTarget::User => config.user_char_limit,
    };
    usize::try_from(limit).unwrap_or(usize::MAX)
}

/// The config key that enables `target`.
#[must_use]
pub const fn flag(target: MemoryTarget) -> &'static str {
    match target {
        MemoryTarget::Memory => "memory.memory_enabled",
        MemoryTarget::User => "memory.user_profile_enabled",
    }
}

/// Whether `entry` is in `target`'s prompt snapshot and budget: a core
/// entry that is not dormant (blocked entries keep their place, shown as a
/// marker).
#[must_use]
pub fn in_core(entry: &MemoryEntry, target: MemoryTarget) -> bool {
    entry.target == target
        && entry.tier == MemoryTier::Core
        && entry.status != MemoryStatus::Dormant
}

/// Characters of `target`'s core tier: its entries joined by [`SEPARATOR`].
#[must_use]
pub fn core_chars(entries: &[MemoryEntry], target: MemoryTarget) -> usize {
    let (count, chars) = entries
        .iter()
        .filter(|e| in_core(e, target))
        .fold((0usize, 0usize), |(n, c), e| {
            (n + 1, c + e.content.chars().count())
        });
    chars + count.saturating_sub(1) * SEPARATOR.chars().count()
}

/// A target's usage, as tool results report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Usage {
    /// Core characters used.
    pub chars: usize,
    /// The budget.
    pub limit: usize,
    /// `chars` as a whole percentage of `limit`.
    pub percent: usize,
}

impl Usage {
    /// The usage of `chars` against `limit`.
    #[must_use]
    pub fn new(chars: usize, limit: usize) -> Self {
        let percent = (chars * 100).checked_div(limit).unwrap_or(100);
        Self {
            chars,
            limit,
            percent,
        }
    }

    /// `[<pct>% — <current>/<limit> chars]`.
    #[must_use]
    pub fn indicator(&self) -> String {
        format!("[{}% — {}/{} chars]", self.percent, self.chars, self.limit)
    }
}
