//! The memory blocks of the system prompt (spec: memory, Frozen
//! system-prompt snapshot; Snapshot refresh only at compression or new
//! session; Threat scanning of memory content).
//!
//! [`render`] reads the store once; the caller puts the text in
//! `PromptSetup::memory` at session start and in the prompt refresher, and
//! the turn loop freezes it with the rest of the prompt, so a write during
//! the session never changes the prompt's bytes until a compaction or a
//! `--now` rebuild asks for it.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use jiff::Timestamp;
use pmt_config::schema::MemoryConfig;
use pmt_core::error::Error;
use pmt_store::{MemoryEntry, MemoryStatus, MemoryTarget, Store};

use crate::budget::{self, SEPARATOR, Usage};
use crate::scan;

/// The heading of `target`'s block.
#[must_use]
pub const fn heading(target: MemoryTarget) -> &'static str {
    match target {
        MemoryTarget::Memory => "MEMORY (your personal notes)",
        MemoryTarget::User => "USER PROFILE (who the user is)",
    }
}

/// An entry as the model may see it: its content, or `[BLOCKED: <rule>]`
/// when it matches the current threat patterns (with that rule).
#[must_use]
pub fn shown(entry: &MemoryEntry) -> (String, Option<&'static str>) {
    match scan::threats(&entry.content).first() {
        Some(rule) => (scan::blocked(rule), Some(*rule)),
        None => (entry.content.clone(), None),
    }
}

/// Whether `entry` has expired at `now`.
#[must_use]
pub fn expired(entry: &MemoryEntry, now: Timestamp) -> bool {
    entry.expires_at.is_some_and(|at| at <= now)
}

/// Entries whose status the scan changed: to block, to unblock.
#[derive(Debug, Default)]
pub(crate) struct Verdicts {
    pub(crate) block: BTreeSet<String>,
    pub(crate) unblock: BTreeSet<String>,
}

impl Verdicts {
    /// Notes what showing `entry` found.
    pub(crate) fn observe(&mut self, entry: &MemoryEntry, rule: Option<&str>) {
        match (rule.is_some(), entry.status) {
            (true, MemoryStatus::Active) => {
                self.block.insert(entry.id.clone());
            }
            (false, MemoryStatus::Blocked) => {
                self.unblock.insert(entry.id.clone());
            }
            _ => {}
        }
    }

    /// Records the verdicts (raw content is kept for `memory show`).
    pub(crate) fn save(self, store: &Store) -> Result<(), Error> {
        if self.block.is_empty() && self.unblock.is_empty() {
            return Ok(());
        }
        store.memory_batch(None, |batch| {
            for entry in &mut batch.entries {
                if self.block.contains(&entry.id) {
                    entry.status = MemoryStatus::Blocked;
                } else if self.unblock.contains(&entry.id) {
                    entry.status = MemoryStatus::Active;
                }
            }
            Ok(())
        })
    }
}

/// The memory section of a new prompt: one block per enabled target that
/// has core entries; `None` when there is nothing to show.
///
/// # Errors
///
/// Store errors.
pub fn render(store: &Store, config: &MemoryConfig) -> Result<Option<String>, Error> {
    let entries = store.memory_entries(None)?;
    let now = Timestamp::now();
    let mut verdicts = Verdicts::default();
    let mut blocks = Vec::new();
    for target in budget::enabled(config) {
        let core: Vec<&MemoryEntry> = entries
            .iter()
            .filter(|e| budget::in_core(e, target) && !expired(e, now))
            .collect();
        if core.is_empty() {
            continue;
        }
        let shown: Vec<String> = core
            .iter()
            .map(|entry| {
                let (text, rule) = shown(entry);
                verdicts.observe(entry, rule);
                text
            })
            .collect();
        let usage = Usage::new(
            budget::core_chars(&entries, target),
            budget::limit(config, target),
        );
        let mut block = String::new();
        let _ = write!(
            block,
            "{} {}\n{}",
            heading(target),
            usage.indicator(),
            shown.join(SEPARATOR)
        );
        blocks.push(block);
    }
    verdicts.save(store)?;
    Ok((!blocks.is_empty()).then(|| blocks.join("\n\n")))
}
