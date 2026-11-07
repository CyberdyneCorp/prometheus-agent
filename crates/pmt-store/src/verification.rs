//! Verification evidence (spec: execution-environments, Project run-recipe
//! verification) and the turn count file snapshots are keyed by.

use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::params;

use crate::error::sql;
use crate::sessions::now_ms;
use crate::store::Store;

/// One recorded `prometheus verify` run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationRun {
    /// When, Unix milliseconds.
    pub at: i64,
    /// Every phase passed.
    pub passed: bool,
    /// The phases' outcomes (JSON).
    pub summary: String,
}

impl Store {
    /// Records a `prometheus verify` outcome as evidence of `session`.
    ///
    /// # Errors
    ///
    /// `store.schema_too_new`, `store.sqlite`.
    pub fn record_verification(
        &self,
        session: SessionId,
        passed: bool,
        summary: &str,
    ) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO verification_runs (session_id, at, passed, summary) VALUES (?1, ?2, ?3, ?4)",
                params![session.to_string(), now_ms(), passed, summary],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// `session`'s verification runs, oldest first.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn verifications(&self, session: SessionId) -> Result<Vec<VerificationRun>, Error> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT at, passed, summary FROM verification_runs WHERE session_id = ?1 ORDER BY at, id",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([session.to_string()], |row| {
                    Ok(VerificationRun {
                        at: row.get(0)?,
                        passed: row.get(1)?,
                        summary: row.get(2)?,
                    })
                })
                .map_err(sql)?;
            rows.collect::<Result<_, _>>().map_err(sql)
        })
    }

    /// How many user messages `session` ever had (soft-deleted ones
    /// included): its current turn number, which only grows.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn user_turns(&self, session: SessionId) -> Result<u64, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND role = 'user'",
                [session.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| u64::try_from(n).unwrap_or_default())
            .map_err(sql)
        })
    }
}

impl Store {
    /// Records one verify-on-stop evidence row (`edit` or `check`) of
    /// `turn` in `session` (spec: agent-turn-loop, Verify-on-stop).
    ///
    /// # Errors
    ///
    /// `store.schema_too_new`, `store.sqlite`.
    pub fn record_verify_evidence(
        &self,
        session: SessionId,
        turn: &str,
        kind: &str,
        detail: &str,
    ) -> Result<(), Error> {
        let session = session.to_string();
        self.write(|tx| {
            tx.execute(
                "INSERT INTO verify_ledger (session_id, turn_id, kind, detail, at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![session, turn, kind, detail, now_ms()],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// The verify-on-stop ledger of `session` as `(turn, kind, detail)`,
    /// oldest first.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn verify_ledger(
        &self,
        session: SessionId,
    ) -> Result<Vec<(String, String, String)>, Error> {
        let session = session.to_string();
        self.read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT turn_id, kind, detail FROM verify_ledger WHERE session_id = ?1 ORDER BY id",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([&session], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(sql)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(sql)
        })
    }
}
