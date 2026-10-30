//! Retention, pruning and auto-archive (spec: session-storage, Retention,
//! pruning and auto-archive).
//!
//! A session is prunable when it ended, was inactive for longer than the
//! retention period, is not pinned and has no live lease. A compression
//! chain (a session and its compression ancestors and continuations) is
//! pruned only as a whole, together with the `delegate` children deletion
//! cascades to, and only when every one of those sessions is prunable. Maintenance runs
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

/// The first session of the compression chain `id` belongs to.
fn chain_head(conn: &Connection, id: &str) -> Result<String, Error> {
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
    Ok(first)
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

/// One deletion retention may run: a chain head with everything its
/// deletion removes (`doomed`, head first) and the children it re-parents.
struct Plan {
    doomed: Vec<String>,
    kept: Vec<String>,
}

/// The deletions retention runs. Deleting a chain head cascades to its
/// whole compression chain and to `delegate` children (and theirs), so a
/// chain is pruned only when every session that cascade removes is itself
/// prunable and unleased; otherwise it is left whole. A plan another plan
/// already contains (a prunable delegate child of a prunable parent) is
/// dropped, so dry run and prune list each session once.
fn prunable_plans(conn: &Connection, cutoff: i64) -> Result<Vec<Plan>, Error> {
    let eligible = candidates(conn, cutoff)?;
    let mut heads = BTreeSet::new();
    for id in &eligible {
        heads.insert(chain_head(conn, id)?);
    }
    let mut plans = Vec::new();
    for head in heads {
        let (doomed, kept) = delete::plan(conn, SessionId::parse(&head)?, true)?;
        if all_prunable(conn, &eligible, &doomed)? {
            plans.push(Plan { doomed, kept });
        }
    }
    let nested: HashSet<String> = plans
        .iter()
        .flat_map(|plan| plan.doomed[1..].iter().cloned())
        .collect();
    plans.retain(|plan| !nested.contains(&plan.doomed[0]));
    Ok(plans)
}

fn all_prunable(
    conn: &Connection,
    eligible: &HashSet<String>,
    ids: &[String],
) -> Result<bool, Error> {
    for id in ids {
        if !eligible.contains(id) || has_live_lease(conn, id)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Sessions a prune deleted and the blob hashes they referenced.
type PrunedPlan = (Vec<String>, BTreeSet<String>);

/// Runs one plan; a session that became busy since planning skips the
/// plan, not the prune.
fn prune_plan(tx: &Transaction<'_>, plan: Plan) -> Result<Option<PrunedPlan>, Error> {
    match delete::delete_planned(tx, &plan.doomed, &plan.kept) {
        Ok(blobs) => Ok(Some((plan.doomed, blobs))),
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
            let plans = self.read(|conn| prunable_plans(conn, cutoff))?;
            return plans
                .iter()
                .flat_map(|plan| &plan.doomed)
                .map(|id| SessionId::parse(id))
                .collect();
        }
        let started = SystemTime::now();
        let (deleted, blobs) = self.write(|tx| {
            let mut deleted = Vec::new();
            let mut blobs = BTreeSet::new();
            for plan in prunable_plans(tx, cutoff)? {
                if let Some((doomed, found)) = prune_plan(tx, plan)? {
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
