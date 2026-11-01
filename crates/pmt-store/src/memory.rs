//! Built-in memory rows (spec: memory, Built-in store of typed entries;
//! Atomic batches judged on the final state; Write approval and pending
//! changes).
//!
//! The store only persists: what a batch may do (budgets, matching,
//! scanning) belongs to `pmt-memory`. A batch is one `BEGIN IMMEDIATE`
//! transaction: [`Store::memory_batch`] loads the target's entries and the
//! staged changes, lets the caller edit them in memory, and writes the
//! difference, so concurrent sessions never interleave half a batch and a
//! rule checked on the loaded state still holds at commit.

use std::collections::BTreeMap;

use jiff::Timestamp;
use pmt_core::error::Error;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde_json::Value;

use crate::error::{corrupt, sql};
use crate::model::{MemoryKind, MemoryOrigin, MemoryStatus, MemoryTarget, MemoryTier};
use crate::sessions::now_ms;
use crate::store::Store;

/// One memory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    /// `m_` plus 12 base32 characters.
    pub id: String,
    /// The store it belongs to.
    pub target: MemoryTarget,
    /// What it records.
    pub kind: MemoryKind,
    /// Prompt snapshot or recall.
    pub tier: MemoryTier,
    /// The text.
    pub content: String,
    /// Pinned entries never decay and are never consolidated away.
    pub pinned: bool,
    /// Lifecycle state.
    pub status: MemoryStatus,
    /// When it stops applying.
    pub expires_at: Option<Timestamp>,
    /// Created.
    pub created_at: Timestamp,
    /// Last changed.
    pub updated_at: Timestamp,
    /// Last recalled into a turn.
    pub last_recalled_at: Option<Timestamp>,
    /// Times recalled.
    pub recall_count: u64,
    /// Who wrote it.
    pub origin: MemoryOrigin,
    /// The session that wrote it, when one did.
    pub origin_session: Option<String>,
}

/// A staged memory change awaiting `/memory approve`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryPending {
    /// The pending id.
    pub id: String,
    /// The store the change applies to.
    pub target: MemoryTarget,
    /// The operations (a JSON array with resolved entry ids).
    pub operations: Value,
    /// Content of every touched entry when staged (`{id: content}`).
    pub base: Value,
    /// Who proposed it.
    pub origin: MemoryOrigin,
    /// The proposing session.
    pub session_id: Option<String>,
    /// Staged.
    pub created_at: Timestamp,
}

/// The state one [`Store::memory_batch`] works on.
#[derive(Debug, Clone, Default)]
pub struct MemoryBatch {
    /// The entries of the batch's target (all targets for `None`): edit,
    /// push or remove them; the difference is written at commit.
    pub entries: Vec<MemoryEntry>,
    /// Every staged change (read-only).
    pub pending: Vec<MemoryPending>,
    /// Changes to stage in the same transaction.
    pub stage: Vec<MemoryPending>,
    /// Staged changes to drop in the same transaction (approved or
    /// rejected).
    pub resolve: Vec<String>,
}

const COLUMNS: &str = "id, target, kind, tier, content, pinned, status, expires_at, created_at, \
     updated_at, last_recalled_at, recall_count, origin, origin_session";

fn time(row: &Row<'_>, index: usize, column: &str) -> Result<Option<Timestamp>, Error> {
    let ms: Option<i64> = row.get(index).map_err(sql)?;
    ms.map(|ms| Timestamp::from_millisecond(ms).map_err(|e| corrupt(column, e)))
        .transpose()
}

fn required_time(row: &Row<'_>, index: usize, column: &str) -> Result<Timestamp, Error> {
    time(row, index, column)?.ok_or_else(|| corrupt(column, "missing"))
}

fn text<T: std::str::FromStr<Err = Error>>(row: &Row<'_>, index: usize) -> Result<T, Error> {
    row.get::<_, String>(index).map_err(sql)?.parse()
}

fn entry_from_row(row: &Row<'_>) -> Result<MemoryEntry, Error> {
    let count: i64 = row.get(11).map_err(sql)?;
    Ok(MemoryEntry {
        id: row.get(0).map_err(sql)?,
        target: text(row, 1)?,
        kind: text(row, 2)?,
        tier: text(row, 3)?,
        content: row.get(4).map_err(sql)?,
        pinned: row.get::<_, i64>(5).map_err(sql)? != 0,
        status: text(row, 6)?,
        expires_at: time(row, 7, "memory_entries.expires_at")?,
        created_at: required_time(row, 8, "memory_entries.created_at")?,
        updated_at: required_time(row, 9, "memory_entries.updated_at")?,
        last_recalled_at: time(row, 10, "memory_entries.last_recalled_at")?,
        recall_count: u64::try_from(count)
            .map_err(|e| corrupt("memory_entries.recall_count", e))?,
        origin: text(row, 12)?,
        origin_session: row.get(13).map_err(sql)?,
    })
}

fn pending_from_row(row: &Row<'_>) -> Result<MemoryPending, Error> {
    let json = |index: usize, column: &str| -> Result<Value, Error> {
        let raw: String = row.get(index).map_err(sql)?;
        serde_json::from_str(&raw).map_err(|e| corrupt(column, e))
    };
    Ok(MemoryPending {
        id: row.get(0).map_err(sql)?,
        target: text(row, 1)?,
        operations: json(2, "memory_pending.operations")?,
        base: json(3, "memory_pending.base")?,
        origin: text(row, 4)?,
        session_id: row.get(5).map_err(sql)?,
        created_at: required_time(row, 6, "memory_pending.created_at")?,
    })
}

fn load_entries(
    conn: &Connection,
    target: Option<MemoryTarget>,
) -> Result<Vec<MemoryEntry>, Error> {
    let filter = if target.is_some() {
        "WHERE target = ?1"
    } else {
        "WHERE ?1 IS NULL"
    };
    let statement =
        format!("SELECT {COLUMNS} FROM memory_entries {filter} ORDER BY created_at, id");
    let mut stmt = conn.prepare_cached(&statement).map_err(sql)?;
    let mut rows = stmt
        .query([target.map(MemoryTarget::as_str)])
        .map_err(sql)?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next().map_err(sql)? {
        entries.push(entry_from_row(row)?);
    }
    Ok(entries)
}

fn load_pending(conn: &Connection) -> Result<Vec<MemoryPending>, Error> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT id, target, operations, base, origin, session_id, created_at \
             FROM memory_pending ORDER BY created_at, id",
        )
        .map_err(sql)?;
    let mut rows = stmt.query([]).map_err(sql)?;
    let mut pending = Vec::new();
    while let Some(row) = rows.next().map_err(sql)? {
        pending.push(pending_from_row(row)?);
    }
    Ok(pending)
}

fn ms(at: Timestamp) -> i64 {
    at.as_millisecond()
}

fn upsert(tx: &Transaction<'_>, e: &MemoryEntry) -> Result<(), Error> {
    tx.execute(
        &format!(
            "INSERT INTO memory_entries ({COLUMNS}) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \
             ON CONFLICT (id) DO UPDATE SET target = excluded.target, kind = excluded.kind, \
             tier = excluded.tier, content = excluded.content, pinned = excluded.pinned, \
             status = excluded.status, expires_at = excluded.expires_at, \
             updated_at = excluded.updated_at, last_recalled_at = excluded.last_recalled_at, \
             recall_count = excluded.recall_count"
        ),
        params![
            e.id,
            e.target.as_str(),
            e.kind.as_str(),
            e.tier.as_str(),
            e.content,
            i64::from(e.pinned),
            e.status.as_str(),
            e.expires_at.map(ms),
            ms(e.created_at),
            ms(e.updated_at),
            e.last_recalled_at.map(ms),
            i64::try_from(e.recall_count).unwrap_or(i64::MAX),
            e.origin.as_str(),
            e.origin_session,
        ],
    )
    .map_err(sql)?;
    Ok(())
}

fn insert_pending(tx: &Transaction<'_>, p: &MemoryPending) -> Result<(), Error> {
    tx.execute(
        "INSERT INTO memory_pending (id, target, operations, base, origin, session_id, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            p.id,
            p.target.as_str(),
            p.operations.to_string(),
            p.base.to_string(),
            p.origin.as_str(),
            p.session_id,
            ms(p.created_at),
        ],
    )
    .map_err(sql)?;
    Ok(())
}

/// Writes what `batch` changed relative to `before`.
fn write_difference(
    tx: &Transaction<'_>,
    before: &[MemoryEntry],
    batch: &MemoryBatch,
) -> Result<(), Error> {
    let old: BTreeMap<&str, &MemoryEntry> = before.iter().map(|e| (e.id.as_str(), e)).collect();
    let kept: BTreeMap<&str, &MemoryEntry> =
        batch.entries.iter().map(|e| (e.id.as_str(), e)).collect();
    for id in old.keys().filter(|id| !kept.contains_key(*id)) {
        tx.execute("DELETE FROM memory_entries WHERE id = ?1", [id])
            .map_err(sql)?;
    }
    for entry in &batch.entries {
        if old.get(entry.id.as_str()) != Some(&entry) {
            upsert(tx, entry)?;
        }
    }
    for id in &batch.resolve {
        tx.execute("DELETE FROM memory_pending WHERE id = ?1", [id])
            .map_err(sql)?;
    }
    for pending in &batch.stage {
        insert_pending(tx, pending)?;
    }
    Ok(())
}

impl Store {
    /// The entries of `target` (every target for `None`), oldest first.
    ///
    /// # Errors
    ///
    /// `store.sqlite` (also for a row that does not parse).
    pub fn memory_entries(&self, target: Option<MemoryTarget>) -> Result<Vec<MemoryEntry>, Error> {
        self.read(|conn| load_entries(conn, target))
    }

    /// The entry `id`, if any.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn memory_entry(&self, id: &str) -> Result<Option<MemoryEntry>, Error> {
        self.read(|conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM memory_entries WHERE id = ?1"),
                [id],
                |row| Ok(entry_from_row(row)),
            )
            .optional()
            .map_err(sql)?
            .transpose()
        })
    }

    /// Every staged change, oldest first.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn memory_pending(&self) -> Result<Vec<MemoryPending>, Error> {
        self.read(load_pending)
    }

    /// Runs `work` on the entries of `target` (all for `None`) and the
    /// staged changes in one write transaction, then writes what it
    /// changed. Nothing is written when `work` fails.
    ///
    /// # Errors
    ///
    /// `work`'s error; `store.sqlite`, `store.busy`.
    pub fn memory_batch<T>(
        &self,
        target: Option<MemoryTarget>,
        mut work: impl FnMut(&mut MemoryBatch) -> Result<T, Error>,
    ) -> Result<T, Error> {
        self.write(|tx| {
            let before = load_entries(tx, target)?;
            let mut batch = MemoryBatch {
                entries: before.clone(),
                pending: load_pending(tx)?,
                stage: Vec::new(),
                resolve: Vec::new(),
            };
            let value = work(&mut batch)?;
            write_difference(tx, &before, &batch)?;
            Ok(value)
        })
    }

    /// Records that `ids` were recalled into a turn now.
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.busy`.
    pub fn record_memory_recall(&self, ids: &[String]) -> Result<(), Error> {
        if ids.is_empty() {
            return Ok(());
        }
        let now = now_ms();
        self.write(|tx| {
            for id in ids {
                tx.execute(
                    "UPDATE memory_entries SET last_recalled_at = ?2, \
                     recall_count = recall_count + 1 WHERE id = ?1",
                    params![id, now],
                )
                .map_err(sql)?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, content: &str) -> MemoryEntry {
        let now = Timestamp::now();
        MemoryEntry {
            id: id.to_owned(),
            target: MemoryTarget::User,
            kind: MemoryKind::Fact,
            tier: MemoryTier::Core,
            content: content.to_owned(),
            pinned: false,
            status: MemoryStatus::Active,
            expires_at: None,
            created_at: now,
            updated_at: now,
            last_recalled_at: None,
            recall_count: 0,
            origin: MemoryOrigin::User,
            origin_session: None,
        }
    }

    fn store() -> (tempfile::TempDir, Store) {
        let root = tempfile::tempdir().unwrap();
        let home = pmt_config::Home::open(&root.path().join("home")).unwrap();
        let dir = home
            .profile(&pmt_config::ProfileName::parse("default").unwrap())
            .unwrap();
        let scope = pmt_config::ProfileScope::open(
            dir,
            &pmt_config::Overrides::parse::<&str>(&[]).unwrap(),
            pmt_config::Launch::Other,
        )
        .unwrap();
        let store = Store::open(&scope).unwrap();
        (root, store)
    }

    #[test]
    fn a_batch_writes_its_difference_and_a_failure_writes_nothing() {
        let (_root, store) = store();
        store
            .memory_batch(Some(MemoryTarget::User), |b| {
                b.entries.push(entry("m_aaaaaaaaaaaa", "one"));
                b.entries.push(entry("m_bbbbbbbbbbbb", "two"));
                Ok(())
            })
            .unwrap();
        let failed: Result<(), Error> = store.memory_batch(Some(MemoryTarget::User), |b| {
            b.entries.clear();
            Err(Error::new(&pmt_core::error::codes::MEMORY_INVALID, "no"))
        });
        assert!(failed.is_err());
        assert_eq!(store.memory_entries(None).unwrap().len(), 2);
        store
            .memory_batch(Some(MemoryTarget::User), |b| {
                b.entries.retain(|e| e.id != "m_aaaaaaaaaaaa");
                b.entries[0].content = "TWO".into();
                Ok(())
            })
            .unwrap();
        let left = store.memory_entries(Some(MemoryTarget::User)).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].content, "TWO");
        assert!(
            store
                .memory_entries(Some(MemoryTarget::Memory))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn staged_changes_round_trip_and_resolve() {
        let (_root, store) = store();
        store
            .memory_batch(None, |b| {
                b.stage.push(MemoryPending {
                    id: "p1".into(),
                    target: MemoryTarget::Memory,
                    operations: serde_json::json!([{"action": "remove", "id": "m_x"}]),
                    base: serde_json::json!({"m_x": "old"}),
                    origin: MemoryOrigin::BackgroundReview,
                    session_id: None,
                    created_at: Timestamp::now(),
                });
                Ok(())
            })
            .unwrap();
        let pending = store.memory_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].base["m_x"], "old");
        store
            .memory_batch(None, |b| {
                b.resolve.push("p1".into());
                Ok(())
            })
            .unwrap();
        assert!(store.memory_pending().unwrap().is_empty());
    }
}
