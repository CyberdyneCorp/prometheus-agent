//! Salience decay, expiry and dormancy (spec: memory, Decay, expiry and
//! dormancy).
//!
//! An unpinned entry's salience is `0.5^(age / half_life)`, where age runs
//! from its last use (the later of `updated_at` and `last_recalled_at`).
//! Recall multiplies relevance by it. An archival entry whose salience
//! falls below `memory.decay.dormant_below`, and any entry past
//! `expires_at`, becomes `dormant`: left out of recall and the snapshot but
//! never deleted. Every function takes `now`, so tests drive the clock.

use jiff::Timestamp;
use pmt_config::schema::MemoryDecayConfig;
use pmt_core::error::{Error, codes};
use pmt_store::{Author, MemoryEntry, MemoryStatus, MemoryTier, Store};

/// When `entry` was last used.
#[must_use]
pub fn last_use(entry: &MemoryEntry) -> Timestamp {
    entry
        .last_recalled_at
        .map_or(entry.updated_at, |r| r.max(entry.updated_at))
}

/// The salience of `entry` at `now`, in `(0, 1]`; pinned entries keep 1.
#[must_use]
pub fn salience(entry: &MemoryEntry, now: Timestamp, config: &MemoryDecayConfig) -> f64 {
    if entry.pinned {
        return 1.0;
    }
    let age_ms = now.duration_since(last_use(entry)).as_millis().max(0);
    let half_life_ms = u128::from(config.half_life_secs.max(1)) * 1000;
    // Lossy on purpose: the ratio only needs a few significant digits.
    #[allow(clippy::cast_precision_loss)]
    let halvings = age_ms as f64 / half_life_ms as f64;
    0.5_f64.powf(halvings)
}

/// Whether `entry` has expired at `now`.
#[must_use]
pub fn expired(entry: &MemoryEntry, now: Timestamp) -> bool {
    entry.expires_at.is_some_and(|at| at <= now)
}

/// Whether `entry` should be dormant at `now`: expired, or an unpinned
/// archival entry whose salience fell below the threshold.
#[must_use]
pub fn due_dormant(entry: &MemoryEntry, now: Timestamp, config: &MemoryDecayConfig) -> bool {
    expired(entry, now)
        || (entry.tier == MemoryTier::Archival
            && !entry.pinned
            && salience(entry, now, config) < config.dormant_below)
}

/// Marks every active entry that is [`due_dormant`] at `now` dormant (a
/// lifecycle change, not a memory edit: nothing is journaled); the ids it
/// changed.
///
/// # Errors
///
/// Store errors.
pub fn sweep(
    store: &Store,
    config: &MemoryDecayConfig,
    now: Timestamp,
) -> Result<Vec<String>, Error> {
    let due: Vec<String> = store
        .memory_entries(None)?
        .iter()
        .filter(|e| e.status == MemoryStatus::Active && due_dormant(e, now, config))
        .map(|e| e.id.clone())
        .collect();
    if due.is_empty() {
        return Ok(due);
    }
    store.memory_batch(None, |batch| {
        for entry in &mut batch.entries {
            if due.contains(&entry.id) && entry.status == MemoryStatus::Active {
                entry.status = MemoryStatus::Dormant;
            }
        }
        Ok(())
    })?;
    Ok(due)
}

/// Makes dormant entry `id` active again (`prometheus memory wake`): its
/// last use becomes `now` and a past expiry is cleared, so it does not
/// fall dormant again at once. Journaled as a user change.
///
/// # Errors
///
/// `memory.not_found`; `memory.invalid` for an entry that is not dormant;
/// store errors.
pub fn wake(store: &Store, id: &str, now: Timestamp) -> Result<MemoryEntry, Error> {
    store.memory_batch(None, |batch| {
        batch.author = Some(Author::user());
        let entry = batch
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| {
                Error::new(&codes::MEMORY_NOT_FOUND, format!("no memory entry '{id}'"))
                    .with_hint("prometheus memory list --all")
            })?;
        if entry.status != MemoryStatus::Dormant {
            return Err(Error::new(
                &codes::MEMORY_INVALID,
                format!("memory entry '{id}' is {}, not dormant", entry.status),
            ));
        }
        entry.status = MemoryStatus::Active;
        entry.last_recalled_at = Some(now);
        if expired(entry, now) {
            entry.expires_at = None;
        }
        Ok(entry.clone())
    })
}

#[cfg(test)]
mod tests {
    use jiff::SignedDuration;
    use pmt_store::{MemoryKind, MemoryOrigin, MemoryTarget};

    use super::*;

    fn entry(updated: Timestamp) -> MemoryEntry {
        MemoryEntry {
            id: "m_aaaaaaaaaaaa".into(),
            target: MemoryTarget::Memory,
            kind: MemoryKind::Fact,
            tier: MemoryTier::Archival,
            content: "x".into(),
            pinned: false,
            status: MemoryStatus::Active,
            expires_at: None,
            created_at: updated,
            updated_at: updated,
            last_recalled_at: None,
            recall_count: 0,
            origin: MemoryOrigin::User,
            origin_session: None,
        }
    }

    #[test]
    fn salience_halves_every_half_life_and_pins_hold() {
        let config = MemoryDecayConfig::default();
        let t0 = Timestamp::from_second(1_700_000_000).unwrap();
        let later = t0
            .checked_add(SignedDuration::from_secs(7_776_000))
            .unwrap();
        let mut e = entry(t0);
        assert!((salience(&e, t0, &config) - 1.0).abs() < 1e-9);
        assert!((salience(&e, later, &config) - 0.5).abs() < 1e-9);
        e.last_recalled_at = Some(later);
        assert!((salience(&e, later, &config) - 1.0).abs() < 1e-9);
        e.last_recalled_at = None;
        e.pinned = true;
        let far = t0
            .checked_add(SignedDuration::from_hours(24 * 3650))
            .unwrap();
        assert!((salience(&e, far, &config) - 1.0).abs() < 1e-9);
        assert!(!due_dormant(&e, far, &config));
    }
}
