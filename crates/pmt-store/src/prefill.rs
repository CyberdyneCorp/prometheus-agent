//! A session's frozen prefill messages (spec: agent-turn-loop, Prefill
//! messages).

use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use crate::error::{corrupt, sql};
use crate::store::Store;

/// The prefill a session froze at its start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPrefill {
    /// The file it was read from.
    pub path: String,
    /// `sha256:<hex>` of the file's bytes.
    pub sha256: String,
    /// The messages (a JSON array of `{role, content}`).
    pub messages: Value,
}

impl Store {
    /// Freezes `prefill` for `session` (kept for the session's life).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn save_prefill(&self, session: SessionId, prefill: &SessionPrefill) -> Result<(), Error> {
        let session = session.to_string();
        self.write(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO session_prefill (session_id, path, sha256, messages) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    session,
                    prefill.path,
                    prefill.sha256,
                    prefill.messages.to_string()
                ],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// The prefill `session` froze, if any.
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.corrupt`.
    pub fn prefill(&self, session: SessionId) -> Result<Option<SessionPrefill>, Error> {
        let session = session.to_string();
        let row: Option<(String, String, String)> = self.read(|conn| {
            conn.query_row(
                "SELECT path, sha256, messages FROM session_prefill WHERE session_id = ?1",
                [&session],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(sql)
        })?;
        row.map(|(path, sha256, messages)| {
            Ok(SessionPrefill {
                path,
                sha256,
                messages: serde_json::from_str(&messages)
                    .map_err(|e| corrupt("session_prefill.messages", e))?,
            })
        })
        .transpose()
    }
}
