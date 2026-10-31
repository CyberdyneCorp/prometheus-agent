//! Session deletion (spec: session-storage, Deletion semantics).
//!
//! Deleting a session removes its messages, usage-ledger rows, leases and
//! index entries, cascades to its `delegate` children (and to its
//! `compression` continuations only with `with_lineage`), and re-parents
//! every other child to no parent with `parent_deleted = 1`, keeping its
//! `lineage_kind` and `root_session_id` (usage rows stay grouped under the
//! root they were recorded with). Blobs only the deleted rows referenced
//! are removed after the commit, and so are the deleted sessions'
//! tool-result spillover directories (spec: tool-system, Spillover
//! retention).

use std::collections::{BTreeSet, VecDeque};
use std::path::Path;
use std::time::SystemTime;

use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::{Connection, Transaction};

use crate::blobs::BlobRef;
use crate::error::{self, sql};
use crate::index;
use crate::leases;
use crate::sessions;
use crate::store::Store;

/// What a deletion removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Deleted {
    /// Deleted sessions, the requested one first.
    pub sessions: Vec<SessionId>,
    /// Children kept with `parent_deleted = 1`.
    pub reparented: Vec<SessionId>,
    /// Blob files removed.
    pub blobs: usize,
}

/// Children of `id` with their lineage kind.
fn children(conn: &Connection, id: &str) -> Result<Vec<(String, String)>, Error> {
    let mut stmt = conn
        .prepare_cached("SELECT id, lineage_kind FROM sessions WHERE parent_session_id = ?1")
        .map_err(sql)?;
    let rows = stmt
        .query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(sql)?;
    rows.collect::<Result<_, _>>().map_err(sql)
}

/// The sessions a deletion of `root` removes and the children it keeps.
pub(crate) fn plan(
    conn: &Connection,
    root: SessionId,
    with_lineage: bool,
) -> Result<(Vec<String>, Vec<String>), Error> {
    let mut doomed = vec![root.to_string()];
    let mut kept = Vec::new();
    let mut queue = VecDeque::from([root.to_string()]);
    while let Some(id) = queue.pop_front() {
        for (child, kind) in children(conn, &id)? {
            let cascades = kind == "delegate" || (with_lineage && kind == "compression");
            if cascades {
                doomed.push(child.clone());
                queue.push_back(child);
            } else {
                kept.push(child);
            }
        }
    }
    Ok((doomed, kept))
}

fn blobs_of(conn: &Connection, sessions: &[String]) -> Result<BTreeSet<String>, Error> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT DISTINCT r.sha256 FROM blob_refs r JOIN messages m \
             ON r.owner_kind = 'message' AND r.owner_id = m.id WHERE m.session_id = ?1",
        )
        .map_err(sql)?;
    let mut found = BTreeSet::new();
    for session in sessions {
        let rows = stmt
            .query_map([session], |row| row.get::<_, String>(0))
            .map_err(sql)?;
        for sha in rows {
            found.insert(sha.map_err(sql)?);
        }
    }
    Ok(found)
}

fn delete_rows(tx: &Transaction<'_>, session: &str) -> Result<(), Error> {
    index::remove_session(tx, session)?;
    for statement in [
        "DELETE FROM usage_ledger WHERE session_id = ?1",
        "DELETE FROM leases WHERE session_id = ?1",
        "DELETE FROM session_tool_surface WHERE session_id = ?1",
        "DELETE FROM session_todos WHERE session_id = ?1",
        "DELETE FROM session_prompts WHERE session_id = ?1",
        "DELETE FROM session_compression WHERE session_id = ?1",
        "DELETE FROM messages WHERE session_id = ?1",
        "DELETE FROM blob_refs WHERE owner_kind = 'session' AND owner_id = ?1",
        "DELETE FROM sessions WHERE id = ?1",
    ] {
        tx.execute(statement, [session]).map_err(sql)?;
    }
    Ok(())
}

/// Deletes the rows of `doomed` and re-parents `kept`; returns the blob
/// hashes the deleted messages referenced.
pub(crate) fn delete_planned(
    tx: &Transaction<'_>,
    doomed: &[String],
    kept: &[String],
) -> Result<BTreeSet<String>, Error> {
    for id in doomed {
        leases::ensure_free(tx, SessionId::parse(id)?, None)?;
    }
    let blobs = blobs_of(tx, doomed)?;
    for id in doomed {
        delete_rows(tx, id)?;
    }
    for id in kept {
        tx.execute(
            "UPDATE sessions SET parent_session_id = NULL, parent_deleted = 1 WHERE id = ?1",
            [id],
        )
        .map_err(sql)?;
    }
    Ok(blobs)
}

/// Removes the blob files among `candidates` that no row references any
/// more and that were not touched since `since` (a concurrent writer
/// re-storing the same bytes refreshes the mtime).
pub(crate) fn remove_unreferenced(
    store: &Store,
    candidates: &BTreeSet<String>,
    since: SystemTime,
) -> Result<usize, Error> {
    let dir = store.location().blobs();
    let mut removed = 0;
    for sha in candidates {
        let Some(blob) = BlobRef::parse(&format!("blob:sha256:{sha}")) else {
            continue;
        };
        if store.blob_references(&blob)? > 0 {
            continue;
        }
        let path = dir.join(&sha[..2]).join(sha);
        if untouched_since(&path, since) {
            std::fs::remove_file(&path).map_err(|e| error::io(&path, &e))?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Removes the spillover directory of each deleted session. The rows are
/// already gone, so a directory that cannot be removed is logged, not
/// reported as a failed deletion (retention pruning removes it later).
pub(crate) fn remove_spillover(store: &Store, sessions: &[String]) {
    for session in sessions {
        let dir = store.location().spillover().join(session);
        match std::fs::remove_dir_all(&dir) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                tracing::warn!("could not remove {}: {err}", dir.display());
            }
            _ => {}
        }
    }
}

fn untouched_since(path: &Path, since: SystemTime) -> bool {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .is_ok_and(|modified| modified < since)
}

impl Store {
    /// Deletes `id` per the deletion semantics.
    ///
    /// # Errors
    ///
    /// `session.in_use` (a live turn lease or compaction lock on a session
    /// to delete; nothing changed), `session.not_found`, `store.busy`,
    /// `store.io`, `store.sqlite`.
    pub fn delete_session(&self, id: SessionId, with_lineage: bool) -> Result<Deleted, Error> {
        let started = SystemTime::now();
        let (doomed, kept, blobs) = self.write(|tx| {
            sessions::load(tx, id)?.ok_or_else(|| sessions::not_found(id))?;
            let (doomed, kept) = plan(tx, id, with_lineage)?;
            let blobs = delete_planned(tx, &doomed, &kept)?;
            Ok((doomed, kept, blobs))
        })?;
        remove_spillover(self, &doomed);
        let parse = |ids: Vec<String>| -> Result<Vec<SessionId>, Error> {
            ids.iter().map(|id| SessionId::parse(id)).collect()
        };
        Ok(Deleted {
            sessions: parse(doomed)?,
            reparented: parse(kept)?,
            blobs: remove_unreferenced(self, &blobs, started)?,
        })
    }
}
