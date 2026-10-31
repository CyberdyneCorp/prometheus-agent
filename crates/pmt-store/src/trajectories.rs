//! Trajectory retention bookkeeping (spec: observability, Trajectory
//! retention and size limits). The files themselves belong to `pmt-obs`;
//! the store answers which sessions are due and records the purge.

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::params;

use crate::error::sql;
use crate::sessions;
use crate::store::Store;

impl Store {
    /// Sessions whose trajectory the retention sweep deletes: ended before
    /// `cutoff`, not pinned, not purged yet.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn trajectory_purge_candidates(&self, cutoff: Timestamp) -> Result<Vec<SessionId>, Error> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id FROM sessions WHERE ended_at IS NOT NULL AND ended_at < ?1 \
                     AND pinned = 0 AND trajectory_purged_at IS NULL ORDER BY ended_at",
                )
                .map_err(sql)?;
            let ids = stmt
                .query_map(params![cutoff.as_millisecond()], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(sql)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql)?;
            ids.iter().map(|id| SessionId::parse(id)).collect()
        })
    }

    /// Records that the session's trajectory was deleted now.
    ///
    /// # Errors
    ///
    /// `session.not_found`; store errors.
    pub fn mark_trajectory_purged(&self, id: SessionId) -> Result<(), Error> {
        let now = sessions::now_ms();
        self.write(|tx| {
            let changed = tx
                .execute(
                    "UPDATE sessions SET trajectory_purged_at = ?2 WHERE id = ?1",
                    params![id.to_string(), now],
                )
                .map_err(sql)?;
            if changed == 0 {
                return Err(sessions::not_found(id));
            }
            Ok(())
        })
    }
}
