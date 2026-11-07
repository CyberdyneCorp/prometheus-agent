//! The memory revision journal (spec: memory, Memory revision journal;
//! Reviewable memory changes per session).
//!
//! [`Store::memory_batch`] calls [`journal`] inside its transaction with the
//! entries before and after the batch, so a change and its revision commit
//! together. A revision keeps the whole entry on both sides (JSON), which is
//! what `prometheus memory revert` restores.

use std::collections::BTreeMap;

use jiff::Timestamp;
use pmt_core::error::Error;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde_json::{Value, json};

use crate::error::{corrupt, sql};
use crate::memory::MemoryEntry;
use crate::model::{MemoryOrigin, MemoryTarget};
use crate::store::Store;

/// Who made a batch's changes: recorded on each of its revisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Author {
    /// The origin.
    pub origin: MemoryOrigin,
    /// The session, when one made the change.
    pub session: Option<String>,
}

impl Author {
    /// The user, outside any session (CLI).
    #[must_use]
    pub const fn user() -> Self {
        Self {
            origin: MemoryOrigin::User,
            session: None,
        }
    }
}

/// What a revision did to its entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Operation {
    /// The entry was created.
    Add,
    /// Its content changed.
    Patch,
    /// It was deleted.
    Remove,
    /// Another field changed (pin, tier, kind, status, expiry).
    Update,
}

impl Operation {
    /// The stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Patch => "patch",
            Self::Remove => "remove",
            Self::Update => "update",
        }
    }

    fn parse(text: &str) -> Result<Self, Error> {
        match text {
            "add" => Ok(Self::Add),
            "patch" => Ok(Self::Patch),
            "remove" => Ok(Self::Remove),
            "update" => Ok(Self::Update),
            other => Err(corrupt("memory_revisions.operation", other)),
        }
    }
}

/// One journaled change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    /// Monotonic per profile.
    pub rev: i64,
    /// The entry.
    pub entry_id: String,
    /// Its store.
    pub target: MemoryTarget,
    /// What happened.
    pub operation: Operation,
    /// The entry before (`None` for an add).
    pub before: Option<MemoryEntry>,
    /// The entry after (`None` for a remove).
    pub after: Option<MemoryEntry>,
    /// Who.
    pub origin: MemoryOrigin,
    /// In which session.
    pub session_id: Option<String>,
    /// When.
    pub at: Timestamp,
    /// Kept or reverted in `/memory review`.
    pub reviewed: bool,
}

/// Which revisions to read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RevisionFilter {
    /// Revisions after this one.
    pub after_rev: Option<i64>,
    /// Revisions at or after this time.
    pub since: Option<Timestamp>,
    /// Only this origin.
    pub origin: Option<MemoryOrigin>,
    /// Only this session.
    pub session: Option<String>,
    /// Only revisions not yet kept or reverted in `/memory review`.
    pub unreviewed: bool,
}

/// The JSON form of `entry` kept in a revision.
#[must_use]
pub fn entry_to_json(entry: &MemoryEntry) -> Value {
    json!({
        "id": entry.id,
        "target": entry.target.as_str(),
        "kind": entry.kind.as_str(),
        "tier": entry.tier.as_str(),
        "content": entry.content,
        "pinned": entry.pinned,
        "status": entry.status.as_str(),
        "expires_at": entry.expires_at.map(Timestamp::as_millisecond),
        "created_at": entry.created_at.as_millisecond(),
        "updated_at": entry.updated_at.as_millisecond(),
        "origin": entry.origin.as_str(),
        "origin_session": entry.origin_session,
    })
}

fn entry_from_json(value: &Value) -> Result<MemoryEntry, Error> {
    let bad = |what: &str| corrupt("memory_revisions.entry", what);
    let text = |key: &str| value[key].as_str().ok_or_else(|| bad(key));
    let at = |key: &str| -> Result<Timestamp, Error> {
        let ms = value[key].as_i64().ok_or_else(|| bad(key))?;
        Timestamp::from_millisecond(ms).map_err(|e| corrupt(key, e))
    };
    Ok(MemoryEntry {
        id: text("id")?.to_owned(),
        target: text("target")?.parse()?,
        kind: text("kind")?.parse()?,
        tier: text("tier")?.parse()?,
        content: text("content")?.to_owned(),
        pinned: value["pinned"].as_bool().unwrap_or(false),
        status: text("status")?.parse()?,
        expires_at: value["expires_at"]
            .as_i64()
            .map(|ms| Timestamp::from_millisecond(ms).map_err(|e| corrupt("expires_at", e)))
            .transpose()?,
        created_at: at("created_at")?,
        updated_at: at("updated_at")?,
        last_recalled_at: None,
        recall_count: 0,
        origin: text("origin")?.parse()?,
        origin_session: value["origin_session"].as_str().map(str::to_owned),
    })
}

/// Whether two versions of an entry differ in what the journal records
/// (recall bookkeeping is not a change).
fn differs(a: &MemoryEntry, b: &MemoryEntry) -> bool {
    a.target != b.target
        || a.kind != b.kind
        || a.tier != b.tier
        || a.content != b.content
        || a.pinned != b.pinned
        || a.status != b.status
        || a.expires_at != b.expires_at
}

fn insert(
    tx: &Transaction<'_>,
    author: &Author,
    operation: Operation,
    before: Option<&MemoryEntry>,
    after: Option<&MemoryEntry>,
) -> Result<(), Error> {
    let entry = after.or(before).expect("a revision has a side");
    tx.execute(
        "INSERT INTO memory_revisions \
         (entry_id, target, operation, before, after, origin, session_id, at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            entry.id,
            entry.target.as_str(),
            operation.as_str(),
            before.map(|e| entry_to_json(e).to_string()),
            after.map(|e| entry_to_json(e).to_string()),
            author.origin.as_str(),
            author.session,
            Timestamp::now().as_millisecond(),
        ],
    )
    .map_err(sql)?;
    Ok(())
}

/// Journals every difference between `before` and `after` (one revision
/// per changed entry) in `tx`.
pub(crate) fn journal(
    tx: &Transaction<'_>,
    author: &Author,
    before: &[MemoryEntry],
    after: &[MemoryEntry],
) -> Result<(), Error> {
    let old: BTreeMap<&str, &MemoryEntry> = before.iter().map(|e| (e.id.as_str(), e)).collect();
    let new: BTreeMap<&str, &MemoryEntry> = after.iter().map(|e| (e.id.as_str(), e)).collect();
    for (id, was) in &old {
        if !new.contains_key(id) {
            insert(tx, author, Operation::Remove, Some(was), None)?;
        }
    }
    for entry in after {
        match old.get(entry.id.as_str()) {
            None => insert(tx, author, Operation::Add, None, Some(entry))?,
            Some(was) if was.content != entry.content => {
                insert(tx, author, Operation::Patch, Some(was), Some(entry))?;
            }
            Some(was) if differs(was, entry) => {
                insert(tx, author, Operation::Update, Some(was), Some(entry))?;
            }
            Some(_) => {}
        }
    }
    Ok(())
}

const COLUMNS: &str =
    "rev, entry_id, target, operation, before, after, origin, session_id, at, reviewed";

fn revision_from_row(row: &Row<'_>) -> Result<Revision, Error> {
    let side = |index: usize| -> Result<Option<MemoryEntry>, Error> {
        let raw: Option<String> = row.get(index).map_err(sql)?;
        raw.map(|raw| {
            let value: Value =
                serde_json::from_str(&raw).map_err(|e| corrupt("memory_revisions.entry", e))?;
            entry_from_json(&value)
        })
        .transpose()
    };
    let at: i64 = row.get(8).map_err(sql)?;
    Ok(Revision {
        rev: row.get(0).map_err(sql)?,
        entry_id: row.get(1).map_err(sql)?,
        target: row.get::<_, String>(2).map_err(sql)?.parse()?,
        operation: Operation::parse(&row.get::<_, String>(3).map_err(sql)?)?,
        before: side(4)?,
        after: side(5)?,
        origin: row.get::<_, String>(6).map_err(sql)?.parse()?,
        session_id: row.get(7).map_err(sql)?,
        at: Timestamp::from_millisecond(at).map_err(|e| corrupt("memory_revisions.at", e))?,
        reviewed: row.get::<_, i64>(9).map_err(sql)? != 0,
    })
}

fn query(conn: &Connection, filter: &RevisionFilter) -> Result<Vec<Revision>, Error> {
    let mut stmt = conn
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM memory_revisions \
             WHERE (?1 IS NULL OR rev > ?1) AND (?2 IS NULL OR at >= ?2) \
             AND (?3 IS NULL OR origin = ?3) AND (?4 IS NULL OR session_id = ?4) \
             AND (?5 = 0 OR reviewed = 0) ORDER BY rev"
        ))
        .map_err(sql)?;
    let mut rows = stmt
        .query(params![
            filter.after_rev,
            filter.since.map(Timestamp::as_millisecond),
            filter.origin.map(MemoryOrigin::as_str),
            filter.session,
            i64::from(filter.unreviewed),
        ])
        .map_err(sql)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(sql)? {
        out.push(revision_from_row(row)?);
    }
    Ok(out)
}

impl Store {
    /// The revisions matching `filter`, oldest first.
    ///
    /// # Errors
    ///
    /// `store.sqlite` (also for a row that does not parse).
    pub fn memory_revisions(&self, filter: &RevisionFilter) -> Result<Vec<Revision>, Error> {
        self.read(|conn| query(conn, filter))
    }

    /// Marks `revs` as kept or reverted in `/memory review`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.busy`.
    pub fn mark_memory_reviewed(&self, revs: &[i64]) -> Result<(), Error> {
        self.write(|tx| {
            for rev in revs {
                tx.execute(
                    "UPDATE memory_revisions SET reviewed = 1 WHERE rev = ?1",
                    [rev],
                )
                .map_err(sql)?;
            }
            Ok(())
        })
    }

    /// Revision `rev`, if any.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn memory_revision(&self, rev: i64) -> Result<Option<Revision>, Error> {
        self.read(|conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM memory_revisions WHERE rev = ?1"),
                [rev],
                |row| Ok(revision_from_row(row)),
            )
            .optional()
            .map_err(sql)?
            .transpose()
        })
    }
}
