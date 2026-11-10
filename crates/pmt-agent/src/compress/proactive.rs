//! Opt-in proactive prune (spec: context-compression, Opt-in proactive
//! prune).
//!
//! Below the compaction trigger, once the context passes
//! `compression.proactive_prune_tokens`, only the deterministic prune runs,
//! and only on tool results over [`PROACTIVE_MIN_CHARS`] characters outside
//! the protected head and tail. It commits (in place, originals kept in
//! the blob store) only when it reclaims at least [`MIN_RECLAIM`] tokens;
//! the caller rearms it only after the context regrows by what it
//! reclaimed, so the cache breaks it causes stay episodic.

use pmt_core::error::Error;
use pmt_core::id::MessageId;
use pmt_store::Message as Row;

use super::assemble::{Assembled, Item, copy};
use super::compactor::{Compactor, Run};
use super::prune::{PROACTIVE_MIN_CHARS, prune_large};
use super::rows_tokens;
use super::validate::validate;

/// The fewest tokens a proactive prune commits for.
pub const MIN_RECLAIM: u64 = 4_096;

/// How a proactive prune ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proactive {
    /// Committed; the tokens reclaimed.
    Committed(u64),
    /// Reclaims too little to break the cache for; the tokens it would
    /// have reclaimed.
    TooLittle(u64),
    /// Another compaction holds the session's lock, or the transcript
    /// changed underneath.
    Skipped,
}

impl Compactor<'_> {
    /// One proactive prune of the session (`run` sizes the protected tail).
    ///
    /// # Errors
    ///
    /// Store failures.
    pub fn proactive_prune(&self, run: &Run) -> Result<Proactive, Error> {
        let Some(lock) = self.lock()? else {
            return Ok(Proactive::Skipped);
        };
        let result = self.prune_locked(run, &lock);
        self.unlock(&lock);
        result
    }

    fn prune_locked(&self, run: &Run, lock: &pmt_store::Lease) -> Result<Proactive, Error> {
        let plan = self.plan(run)?;
        let middle = plan.split.middle.clone();
        let pruned = prune_large(&plan.rows, middle.clone(), PROACTIVE_MIN_CHARS);
        let reclaimed =
            rows_tokens(&plan.rows[middle.clone()]).saturating_sub(rows_tokens(&pruned.rows));
        if reclaimed < MIN_RECLAIM {
            return Ok(Proactive::TooLittle(reclaimed));
        }
        let (items, first_new) = items(&plan.rows, middle.start, &pruned.rows);
        let assembled = Assembled::from_items(&plan.rows, items, first_new);
        if let Err(why) = validate(&assembled) {
            tracing::warn!(reason = %why, "proactive prune refused; nothing changed");
            return Ok(Proactive::Skipped);
        }
        Ok(match self.apply(lock, &assembled, &pruned.originals)? {
            Ok(_) => Proactive::Committed(reclaimed),
            Err(_) => Proactive::Skipped,
        })
    }
}

/// Every row kept, but the pruned ones (`pruned`, the region from
/// `start`) as new rows; the id of the first new row.
fn items(rows: &[Row], start: usize, pruned: &[Row]) -> (Vec<Item>, MessageId) {
    let mut first = None;
    let items = (0..rows.len())
        .map(|i| match i.checked_sub(start).and_then(|k| pruned.get(k)) {
            Some(after) if after.content != rows[i].content => {
                let mut row = copy(&rows[i]);
                row.content.clone_from(&after.content);
                first.get_or_insert(row.id);
                Item::New(Box::new(row))
            }
            _ => Item::Kept(i),
        })
        .collect();
    (items, first.unwrap_or_default())
}
