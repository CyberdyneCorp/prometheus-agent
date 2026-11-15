//! The harness version each session started on (spec: harness-evolution,
//! Harness loaded once per session), and its experience-library domain
//! and frozen lessons layer (spec: experience-library).

use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::{OptionalExtension, params};

use crate::error::sql;
use crate::sessions::{load, not_found};
use crate::store::Store;

impl Store {
    /// The harness version `session` recorded, if any.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn session_harness(&self, session: SessionId) -> Result<Option<String>, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT harness_version FROM sessions WHERE id = ?1",
                [session.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(sql)?
            .ok_or_else(|| not_found(session))
        })
    }

    /// Records the harness version `session` runs on.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn set_session_harness(&self, session: SessionId, version: &str) -> Result<(), Error> {
        self.write(|tx| {
            if load(tx, session)?.is_none() {
                return Err(not_found(session));
            }
            tx.execute(
                "UPDATE sessions SET harness_version = ?2 WHERE id = ?1",
                params![session.to_string(), version],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// Open sessions (not ended) that froze a prompt: the ones an explicit
    /// `--now` harness activation marks for a rebuild.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn open_sessions_with_prompt(&self) -> Result<Vec<SessionId>, Error> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT id FROM sessions WHERE ended_at IS NULL \
                     AND id IN (SELECT session_id FROM session_prompts) ORDER BY id",
                )
                .map_err(sql)?;
            let ids = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sql)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql)?;
            Ok(ids
                .iter()
                .filter_map(|id| SessionId::parse(id).ok())
                .collect())
        })
    }

    /// The experience-library domain tag of `session`.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn session_domain(&self, session: SessionId) -> Result<Option<String>, Error> {
        self.column(session, "domain")
    }

    /// Tags `session` with `domain`.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn set_session_domain(&self, session: SessionId, domain: &str) -> Result<(), Error> {
        self.set_column(session, "domain", domain)
    }

    /// The lessons layer `session` froze at its start (`Some("")`: none
    /// was offered; `None`: not decided yet).
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn session_lessons(&self, session: SessionId) -> Result<Option<String>, Error> {
        self.column(session, "lessons")
    }

    /// Freezes the lessons layer of `session`.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn set_session_lessons(&self, session: SessionId, layer: &str) -> Result<(), Error> {
        self.set_column(session, "lessons", layer)
    }

    fn column(&self, session: SessionId, column: &'static str) -> Result<Option<String>, Error> {
        self.read(|conn| {
            conn.query_row(
                &format!("SELECT {column} FROM sessions WHERE id = ?1"),
                [session.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(sql)?
            .ok_or_else(|| not_found(session))
        })
    }

    fn set_column(
        &self,
        session: SessionId,
        column: &'static str,
        value: &str,
    ) -> Result<(), Error> {
        self.write(|tx| {
            if load(tx, session)?.is_none() {
                return Err(not_found(session));
            }
            tx.execute(
                &format!("UPDATE sessions SET {column} = ?2 WHERE id = ?1"),
                params![session.to_string(), value],
            )
            .map_err(sql)?;
            Ok(())
        })
    }
}
