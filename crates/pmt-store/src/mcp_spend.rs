//! What conversations spent on costed MCP tools (spec: plugin-system, MCP
//! tool costs and spend limits).
//!
//! The total is kept per conversation: the root of the session's lineage,
//! which compression continuations and branches share.

use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::{OptionalExtension, params};

use crate::error::sql;
use crate::store::Store;

impl Store {
    /// The conversation key of `session`: its lineage root.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn conversation_root(&self, session: SessionId) -> Result<SessionId, Error> {
        Ok(self.lineage(session)?.first().copied().unwrap_or(session))
    }

    /// What `session`'s conversation spent on `server`.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn mcp_spent(&self, session: SessionId, server: &str) -> Result<f64, Error> {
        let root = self.conversation_root(session)?.to_string();
        self.read(|conn| {
            conn.query_row(
                "SELECT amount FROM mcp_spend WHERE root_session_id = ?1 AND server = ?2",
                params![root, server],
                |row| row.get::<_, f64>(0),
            )
            .optional()
            .map(Option::unwrap_or_default)
            .map_err(sql)
        })
    }

    /// Adds `amount` to what `session`'s conversation spent on `server`.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.schema_too_new`, `store.sqlite`.
    pub fn add_mcp_spend(
        &self,
        session: SessionId,
        server: &str,
        amount: f64,
    ) -> Result<(), Error> {
        let root = self.conversation_root(session)?.to_string();
        self.write(|tx| {
            tx.execute(
                "INSERT INTO mcp_spend (root_session_id, server, amount) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (root_session_id, server) DO UPDATE SET amount = amount + excluded.amount",
                params![root, server, amount],
            )
            .map(drop)
            .map_err(sql)
        })
    }
}
