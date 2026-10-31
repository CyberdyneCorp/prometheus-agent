//! The `todo` list of each session (spec: agent-turn-loop, todo tool
//! contract: state is session-scoped and persisted with the session).
//!
//! The store keeps the list as the turn loop wrote it (a JSON array) with
//! its revision; the contract (merge rules, caps) belongs to the turn loop.

use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use crate::error::{corrupt, sql};
use crate::sessions::{load, not_found};
use crate::store::Store;

/// A session's stored list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTodos {
    /// Bumped by every write.
    pub revision: u64,
    /// The items, in priority order (a JSON array).
    pub items: Value,
}

impl Store {
    /// Stores (or replaces) the list of `session`.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn save_todos(&self, session: SessionId, todos: &SessionTodos) -> Result<(), Error> {
        let items = todos.items.to_string();
        let revision = i64::try_from(todos.revision).unwrap_or(i64::MAX);
        self.write(|tx| {
            if load(tx, session)?.is_none() {
                return Err(not_found(session));
            }
            tx.execute(
                "INSERT INTO session_todos (session_id, revision, items) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (session_id) DO UPDATE SET revision = excluded.revision, \
                 items = excluded.items",
                params![session.to_string(), revision, items],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// The list `session` stored; `None` when it never wrote one.
    ///
    /// # Errors
    ///
    /// `store.sqlite` (also for a row that does not parse).
    pub fn todos(&self, session: SessionId) -> Result<Option<SessionTodos>, Error> {
        let row = self.read(|conn| {
            conn.query_row(
                "SELECT revision, items FROM session_todos WHERE session_id = ?1",
                [session.to_string()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(sql)
        })?;
        let Some((revision, items)) = row else {
            return Ok(None);
        };
        Ok(Some(SessionTodos {
            revision: u64::try_from(revision).map_err(|e| corrupt("session_todos.revision", e))?,
            items: serde_json::from_str(&items).map_err(|e| corrupt("session_todos.items", e))?,
        }))
    }
}
