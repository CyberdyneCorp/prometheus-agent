//! Per-session automation rows (spec: session-automation, Persisted
//! per-session automation state). The store keeps each kind's state as
//! the engine wrote it (JSON); the contract (what a goal, loop or
//! heartbeat holds) belongs to `pmt-automation`.

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use crate::error::{corrupt, sql};
use crate::sessions::{load, not_found};
use crate::store::Store;

/// One stored automation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationRow {
    /// Its session.
    pub session_id: SessionId,
    /// `goal`, `loop` or `heartbeat`.
    pub kind: String,
    /// The engine's state.
    pub state: Value,
    /// The surface that set it.
    pub owner_surface: String,
    /// Last write (ms since the epoch).
    pub updated_at: i64,
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(String, String, String, String, i64)> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
}

fn parse(raw: (String, String, String, String, i64)) -> Result<AutomationRow, Error> {
    let (session, kind, state, owner_surface, updated_at) = raw;
    Ok(AutomationRow {
        session_id: session
            .parse()
            .map_err(|e| corrupt("automation.session_id", e))?,
        kind,
        state: serde_json::from_str(&state).map_err(|e| corrupt("automation.state", e))?,
        owner_surface,
        updated_at,
    })
}

const COLUMNS: &str = "session_id, kind, state, owner_surface, updated_at";

impl Store {
    /// Stores (or replaces) the `kind` automation of `session`.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn put_automation(
        &self,
        session: SessionId,
        kind: &str,
        state: &Value,
        owner_surface: &str,
    ) -> Result<(), Error> {
        let now = Timestamp::now().as_millisecond();
        self.write(|tx| {
            if load(tx, session)?.is_none() {
                return Err(not_found(session));
            }
            tx.execute(
                "INSERT INTO automation (session_id, kind, state, owner_surface, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (session_id, kind) DO UPDATE SET \
                 state = excluded.state, owner_surface = excluded.owner_surface, \
                 updated_at = excluded.updated_at",
                params![
                    session.to_string(),
                    kind,
                    state.to_string(),
                    owner_surface,
                    now
                ],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// Removes the `kind` automation of `session`; `true` when one was set.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn delete_automation(&self, session: SessionId, kind: &str) -> Result<bool, Error> {
        self.write(|tx| {
            tx.execute(
                "DELETE FROM automation WHERE session_id = ?1 AND kind = ?2",
                params![session.to_string(), kind],
            )
            .map(|n| n > 0)
            .map_err(sql)
        })
    }

    /// Removes every automation of `session` (reset, delete, switching a
    /// conversation away); the number removed.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn clear_automation(&self, session: SessionId) -> Result<usize, Error> {
        self.write(|tx| {
            tx.execute(
                "DELETE FROM automation WHERE session_id = ?1",
                [session.to_string()],
            )
            .map_err(sql)
        })
    }

    /// The `kind` automation of `session`.
    ///
    /// # Errors
    ///
    /// `store.sqlite` (also for a row that does not parse).
    pub fn automation(
        &self,
        session: SessionId,
        kind: &str,
    ) -> Result<Option<AutomationRow>, Error> {
        let raw = self.read(|conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM automation WHERE session_id = ?1 AND kind = ?2"),
                params![session.to_string(), kind],
                row,
            )
            .optional()
            .map_err(sql)
        })?;
        raw.map(parse).transpose()
    }

    /// Every automation of `session`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn automations_of(&self, session: SessionId) -> Result<Vec<AutomationRow>, Error> {
        self.select_automation("WHERE session_id = ?1", &[&session.to_string()])
    }

    /// Every automation of the profile (the daemon restores loops and
    /// heartbeats at start).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn all_automation(&self) -> Result<Vec<AutomationRow>, Error> {
        self.select_automation("", &[])
    }

    fn select_automation(
        &self,
        filter: &str,
        args: &[&dyn rusqlite::ToSql],
    ) -> Result<Vec<AutomationRow>, Error> {
        let raw = self.read(|conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM automation {filter} ORDER BY session_id, kind"
                ))
                .map_err(sql)?;
            let rows = stmt.query_map(args, row).map_err(sql)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(sql)
        })?;
        raw.into_iter().map(parse).collect()
    }
}
