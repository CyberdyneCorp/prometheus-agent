//! Retention, pruning and auto-archive (spec: session-storage, Retention,
//! pruning and auto-archive).
//!
//! A session is prunable when it ended, was inactive for longer than the
//! retention period, is not pinned and has no live lease. A compression
//! chain (a session and its compression ancestors and continuations) is
//! pruned only as a whole, when every member is prunable. Maintenance runs
//! the retention prune, the auto-archive of idle lineage tips and the blob
//! collector at most once per 24 hours.

use std::collections::{BTreeSet, HashSet};
use std::time::{Duration, SystemTime};

use pmt_config::schema::SessionsConfig;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::delete;
use crate::error::sql;
use crate::leases::{self, LeaseKind};
use crate::sessions::now_ms;
use crate::store::Store;

/// Minimum time between two maintenance runs.
pub const MAINTENANCE_EVERY: Duration = Duration::from_hours(24);

/// What one maintenance pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Maintenance {
    /// False when the last run was less than [`MAINTENANCE_EVERY`] ago.
    pub ran: bool,
    /// Sessions deleted by retention.
    pub pruned: Vec<SessionId>,
    /// Lineage tips archived as `auto`.
    pub archived: u64,
    /// Blob files collected.
    pub blobs_collected: usize,
}

fn ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// The prunable sessions (ended, inactive since before `cutoff`, unpinned).
fn candidates(conn: &Connection, cutoff: i64) -> Result<HashSet<String>, Error> {
    let mut stmt = conn
        .prepare(
            "SELECT id FROM sessions WHERE ended_at IS NOT NULL AND pinned = 0 \
             AND last_activity_at < ?1",
        )
        .map_err(sql)?;
    let rows = stmt
        .query_map([cutoff], |row| row.get::<_, String>(0))
        .map_err(sql)?;
    rows.collect::<Result<_, _>>().map_err(sql)
}

/// The compression chain `id` belongs to, its first session first.
fn chain(conn: &Connection, id: &str) -> Result<Vec<String>, Error> {
    let mut first = id.to_owned();
    while let Some(parent) = conn
        .query_row(
            "SELECT parent_session_id FROM sessions WHERE id = ?1 AND lineage_kind = 'compression' \
             AND parent_session_id IS NOT NULL",
            [&first],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql)?
    {
        first = parent;
    }
    let mut members = vec![first.clone()];
    let mut stmt = conn
        .prepare_cached(
            "SELECT id FROM sessions WHERE parent_session_id = ?1 AND lineage_kind = 'compression'",
        )
        .map_err(sql)?;
    let mut i = 0;
    while i < members.len() {
        let next = stmt
            .query_map([&members[i]], |row| row.get::<_, String>(0))
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        members.extend(next);
        i += 1;
    }
    Ok(members)
}

fn has_live_lease(conn: &Connection, id: &str) -> Result<bool, Error> {
    let session = SessionId::parse(id)?;
    for kind in [LeaseKind::Turn, LeaseKind::Compaction] {
        if leases::held_by_other(conn, session, kind, None)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whole chains of prunable sessions, each listed first-session first.
fn prunable_chains(conn: &Connection, cutoff: i64) -> Result<Vec<Vec<String>>, Error> {
    let eligible = candidates(conn, cutoff)?;
    let mut seen = BTreeSet::new();
    let mut chains = Vec::new();
    let mut ordered: Vec<&String> = eligible.iter().collect();
    ordered.sort();
    for id in ordered {
        if seen.contains(id) {
            continue;
        }
        let members = chain(conn, id)?;
        seen.extend(members.iter().cloned());
        if !members.iter().all(|m| eligible.contains(m)) {
            continue;
        }
        let mut leased = false;
        for member in &members {
            leased |= has_live_lease(conn, member)?;
        }
        if !leased {
            chains.push(members);
        }
    }
    Ok(chains)
}

/// Deletes one chain (its first session with its whole compression
/// lineage); a session in use is skipped, not an error.
/// Sessions a prune deleted and the blob hashes they referenced.
type PrunedChain = (Vec<String>, BTreeSet<String>);

fn prune_chain(tx: &Transaction<'_>, first: &str) -> Result<Option<PrunedChain>, Error> {
    let (doomed, kept) = delete::plan(tx, SessionId::parse(first)?, true)?;
    match delete::delete_planned(tx, &doomed, &kept) {
        Ok(blobs) => Ok(Some((doomed, blobs))),
        Err(e) if e.code().code == pmt_core::error::codes::SESSION_IN_USE.code => Ok(None),
        Err(e) => Err(e),
    }
}

impl Store {
    /// Prunes (or with `dry_run` only lists) the sessions retention would
    /// delete for sessions inactive longer than `older_than`. Returns the
    /// deleted (or deletable) session ids.
    ///
    /// # Errors
    ///
    /// `store.busy`, `store.io`, `store.schema_too_new`, `store.sqlite`.
    pub fn prune(&self, older_than: Duration, dry_run: bool) -> Result<Vec<SessionId>, Error> {
        let cutoff = now_ms().saturating_sub(ms(older_than));
        if dry_run {
            let chains = self.read(|conn| prunable_chains(conn, cutoff))?;
            return chains
                .into_iter()
                .flatten()
                .map(|id| SessionId::parse(&id))
                .collect();
        }
        let started = SystemTime::now();
        let (deleted, blobs) = self.write(|tx| {
            let mut deleted = Vec::new();
            let mut blobs = BTreeSet::new();
            for members in prunable_chains(tx, cutoff)? {
                if let Some((doomed, found)) = prune_chain(tx, &members[0])? {
                    deleted.extend(doomed);
                    blobs.extend(found);
                }
            }
            Ok((deleted, blobs))
        })?;
        delete::remove_unreferenced(self, &blobs, started)?;
        deleted.iter().map(|id| SessionId::parse(id)).collect()
    }

    /// Marks lineage tips (sessions without a compression continuation)
    /// idle for longer than `idle` as `archived = "auto"`; pinned and
    /// already archived sessions are left alone. Returns how many changed.
    ///
    /// # Errors
    ///
    /// `store.busy`, `store.schema_too_new`, `store.sqlite`.
    pub fn auto_archive(&self, idle: Duration) -> Result<u64, Error> {
        let cutoff = now_ms().saturating_sub(ms(idle));
        self.write(|tx| {
            let changed = tx
                .execute(
                    "UPDATE sessions SET archived = 'auto' WHERE archived = 'none' AND pinned = 0 \
                     AND last_activity_at < ?1 AND NOT EXISTS (SELECT 1 FROM sessions c \
                     WHERE c.parent_session_id = sessions.id AND c.lineage_kind = 'compression')",
                    [cutoff],
                )
                .map_err(sql)?;
            Ok(u64::try_from(changed).unwrap_or(0))
        })
    }

    /// Runs retention (when `sessions.retention_secs > 0`), auto-archive
    /// (when `sessions.auto_archive_secs > 0`) and blob collection, unless
    /// maintenance already ran within [`MAINTENANCE_EVERY`].
    ///
    /// # Errors
    ///
    /// As [`Store::prune`], [`Store::auto_archive`] and
    /// [`Store::collect_blobs`].
    pub fn maintenance(&self, config: &SessionsConfig) -> Result<Maintenance, Error> {
        if !self.claim_maintenance()? {
            return Ok(Maintenance::default());
        }
        let mut report = Maintenance {
            ran: true,
            ..Maintenance::default()
        };
        if config.retention_secs > 0 {
            report.pruned = self.prune(Duration::from_secs(config.retention_secs), false)?;
        }
        if config.auto_archive_secs > 0 {
            report.archived = self.auto_archive(Duration::from_secs(config.auto_archive_secs))?;
        }
        report.blobs_collected = self.collect_blobs()?.deleted.len();
        Ok(report)
    }

    /// Records this run unless one ran within [`MAINTENANCE_EVERY`].
    fn claim_maintenance(&self) -> Result<bool, Error> {
        let now = now_ms();
        self.write(|tx| {
            let last: Option<String> = tx
                .query_row(
                    "SELECT value FROM store_meta WHERE key = 'last_maintenance_at'",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql)?;
            let last = last.and_then(|v| v.parse::<i64>().ok());
            if last.is_some_and(|last| now.saturating_sub(last) < ms(MAINTENANCE_EVERY)) {
                return Ok(false);
            }
            tx.execute(
                "INSERT INTO store_meta (key, value) VALUES ('last_maintenance_at', ?1) \
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![now.to_string()],
            )
            .map_err(sql)?;
            Ok(true)
        })
    }
}
