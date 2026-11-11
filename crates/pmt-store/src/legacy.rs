//! Ids sessions had in the agent they were imported from (spec:
//! hermes-migration, Sessions imported with lineage; Idempotent
//! incremental re-run): `--resume <old id>` resolves through them and a
//! re-run of an import skips what it already carried.

use std::collections::BTreeMap;

use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::error::sql;
use crate::store::Store;

/// One imported session's origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyLink {
    /// The agent it came from (`hermes`).
    pub source: String,
    /// Its id there.
    pub legacy_id: String,
    /// The Prometheus session.
    pub session: SessionId,
    /// Source messages carried so far.
    pub messages: u64,
}

pub(crate) fn insert(tx: &Transaction<'_>, links: &[LegacyLink]) -> Result<(), Error> {
    for link in links {
        tx.execute(
            "INSERT INTO session_legacy_ids (source, legacy_id, session_id, messages) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (source, legacy_id) DO UPDATE SET session_id = excluded.session_id, \
             messages = excluded.messages",
            params![
                link.source,
                link.legacy_id,
                link.session.to_string(),
                i64::try_from(link.messages).unwrap_or(i64::MAX),
            ],
        )
        .map_err(sql)?;
    }
    Ok(())
}

/// The session imported under `legacy_id` from any source.
pub(crate) fn by_legacy(conn: &Connection, legacy_id: &str) -> Result<Option<SessionId>, Error> {
    let id: Option<String> = conn
        .query_row(
            "SELECT l.session_id FROM session_legacy_ids l JOIN sessions s ON s.id = l.session_id \
             WHERE l.legacy_id = ?1 ORDER BY l.source LIMIT 1",
            [legacy_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql)?;
    id.map(|id| SessionId::parse(&id)).transpose()
}

impl Store {
    /// Every session imported from `source`: its legacy id to the session
    /// and the source messages carried.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn legacy_links(&self, source: &str) -> Result<BTreeMap<String, (SessionId, u64)>, Error> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT l.legacy_id, l.session_id, l.messages FROM session_legacy_ids l \
                     JOIN sessions s ON s.id = l.session_id WHERE l.source = ?1",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([source], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(sql)?;
            let mut links = BTreeMap::new();
            for row in rows {
                let (legacy, session, messages) = row.map_err(sql)?;
                let messages = u64::try_from(messages).unwrap_or(0);
                links.insert(legacy, (SessionId::parse(&session)?, messages));
            }
            Ok(links)
        })
    }

    /// The session imported from `source` under `legacy_id`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn legacy_session(
        &self,
        source: &str,
        legacy_id: &str,
    ) -> Result<Option<SessionId>, Error> {
        Ok(self.legacy_links(source)?.get(legacy_id).map(|(id, _)| *id))
    }
}
