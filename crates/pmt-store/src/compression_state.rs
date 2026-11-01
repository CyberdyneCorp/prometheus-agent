//! The automatic-compaction guards of each session (spec:
//! context-compression, Trigger decision and blocking reasons; Summary
//! failure handling).
//!
//! The store keeps the guards as the compressor wrote them; what they mean
//! (escalation, the breaker rule) belongs to the compressor. A session that
//! never compacted has the default state.

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::{OptionalExtension, params};

use crate::error::{corrupt, sql};
use crate::sessions::{load, not_found};
use crate::store::Store;

/// One session's guards.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompressionState {
    /// Consecutive failed summaries (escalates the cooldown).
    pub summary_failures: u32,
    /// Automatic compaction waits until then after a failed summary.
    pub cooldown_until: Option<Timestamp>,
    /// Automatic compaction waits until then after an attempt that
    /// committed nothing for another reason.
    pub backoff_until: Option<Timestamp>,
    /// Consecutive compactions that reclaimed less than 10%.
    pub ineffective: u32,
    /// Context tokens before the last commit, until the provider's next
    /// reported usage judges it.
    pub pending_before: Option<u64>,
}

fn millis(at: Option<Timestamp>) -> Option<i64> {
    at.map(Timestamp::as_millisecond)
}

fn timestamp(column: &str, ms: Option<i64>) -> Result<Option<Timestamp>, Error> {
    ms.map(|ms| Timestamp::from_millisecond(ms).map_err(|e| corrupt(column, e)))
        .transpose()
}

impl Store {
    /// Stores (or replaces) the guards of `session`.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn save_compression_state(
        &self,
        session: SessionId,
        state: &CompressionState,
    ) -> Result<(), Error> {
        let pending = state
            .pending_before
            .map(|t| i64::try_from(t).unwrap_or(i64::MAX));
        self.write(|tx| {
            if load(tx, session)?.is_none() {
                return Err(not_found(session));
            }
            tx.execute(
                "INSERT INTO session_compression (session_id, summary_failures, cooldown_until, \
                 backoff_until, ineffective, pending_before) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT (session_id) DO UPDATE SET summary_failures = excluded.summary_failures, \
                 cooldown_until = excluded.cooldown_until, backoff_until = excluded.backoff_until, \
                 ineffective = excluded.ineffective, pending_before = excluded.pending_before",
                params![
                    session.to_string(),
                    state.summary_failures,
                    millis(state.cooldown_until),
                    millis(state.backoff_until),
                    state.ineffective,
                    pending,
                ],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// The guards of `session` (the default when it never stored any).
    ///
    /// # Errors
    ///
    /// `store.sqlite` (also for a row that does not parse).
    pub fn compression_state(&self, session: SessionId) -> Result<CompressionState, Error> {
        type Raw = (u32, Option<i64>, Option<i64>, u32, Option<i64>);
        let row: Option<Raw> = self.read(|conn| {
            conn.query_row(
                "SELECT summary_failures, cooldown_until, backoff_until, ineffective, \
                 pending_before FROM session_compression WHERE session_id = ?1",
                [session.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()
            .map_err(sql)
        })?;
        let Some((summary_failures, cooldown, backoff, ineffective, pending)) = row else {
            return Ok(CompressionState::default());
        };
        Ok(CompressionState {
            summary_failures,
            cooldown_until: timestamp("session_compression.cooldown_until", cooldown)?,
            backoff_until: timestamp("session_compression.backoff_until", backoff)?,
            ineffective,
            pending_before: pending
                .map(|t| {
                    u64::try_from(t).map_err(|e| corrupt("session_compression.pending_before", e))
                })
                .transpose()?,
        })
    }
}
