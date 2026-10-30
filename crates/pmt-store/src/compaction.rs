//! Non-destructive in-place compaction (spec: session-storage,
//! Non-destructive in-place compaction; context-compression, Compaction is
//! always in place).
//!
//! The compactor reads the transcript without holding a write transaction
//! (the summarizer call can take minutes), then commits under the session's
//! compaction lock: the summarised rows become `active = 0, compacted = 1`
//! with the new epoch (still readable, searchable and exportable), the
//! replacement rows take their place in `seq` order, and rows appended
//! meanwhile are re-sequenced after them. Every row the compactor read is
//! compared (id and content hash, still active) inside the transaction; any
//! change aborts the whole commit with `store.compaction_conflict`.

use std::collections::HashSet;

use pmt_core::error::{Error, codes};
use pmt_core::id::{MessageId, SessionId};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::error::sql;
use crate::leases::{self, Lease, LeaseKind};
use crate::messages::{self, Message, NewMessage};
use crate::store::Store;

/// One compaction to commit.
#[derive(Debug, Clone, PartialEq)]
pub struct Compaction {
    /// Every message the compactor read, with the content hash it saw.
    pub read: Vec<(MessageId, String)>,
    /// The messages the replacements stand for (a subset of `read`).
    pub replaced: Vec<MessageId>,
    /// The rows inserted after the last replaced row (the summary).
    pub replacements: Vec<NewMessage>,
}

impl Compaction {
    /// A compaction that read `messages` and replaces `replaced` of them
    /// with `replacements`.
    #[must_use]
    pub fn new(
        messages: &[Message],
        replaced: Vec<MessageId>,
        replacements: Vec<NewMessage>,
    ) -> Self {
        Self {
            read: messages
                .iter()
                .map(|m| (m.id, m.content_hash.clone()))
                .collect(),
            replaced,
            replacements,
        }
    }
}

/// A committed compaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compacted {
    /// The session's new compaction epoch (1 for the first).
    pub epoch: u32,
    /// The `seq`s of the inserted replacement rows.
    pub inserted: Vec<u64>,
}

fn conflict(id: MessageId, why: &str) -> Error {
    Error::new(
        &codes::STORE_COMPACTION_CONFLICT,
        format!("message {id} {why} since the compaction read it; nothing changed"),
    )
}

/// Compares every read row with what is stored now.
fn verify_reads(
    conn: &Connection,
    session: SessionId,
    read: &[(MessageId, String)],
) -> Result<(), Error> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT session_id, active, compacted, content_hash FROM messages WHERE id = ?1",
        )
        .map_err(sql)?;
    for (id, hash) in read {
        let row = stmt
            .query_row([id.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .optional()
            .map_err(sql)?;
        match row {
            None => return Err(conflict(*id, "was deleted")),
            Some((owner, ..)) if owner != session.to_string() => {
                return Err(conflict(*id, "belongs to another session"));
            }
            Some((_, active, compacted, _)) if !active || compacted => {
                return Err(conflict(*id, "became inactive"));
            }
            Some((.., stored)) if stored != *hash => return Err(conflict(*id, "was rewritten")),
            Some(_) => {}
        }
    }
    Ok(())
}

fn check_shape(lock: &Lease, compaction: &Compaction) -> Result<(), Error> {
    if lock.kind() != LeaseKind::Compaction {
        return Err(Error::new(
            &codes::STORE_LEASE_LOST,
            "a compaction must be committed under the session's compaction lock",
        ));
    }
    let read: HashSet<MessageId> = compaction.read.iter().map(|(id, _)| *id).collect();
    if let Some(id) = compaction.replaced.iter().find(|id| !read.contains(id)) {
        return Err(Error::new(
            &codes::STORE_CONSTRAINT,
            format!("compaction replaces message {id}, which it did not read"),
        ));
    }
    if compaction.replaced.is_empty() {
        return Err(Error::new(
            &codes::STORE_CONSTRAINT,
            "a compaction must replace at least one message",
        ));
    }
    Ok(())
}

/// Moves every row after `after` up by `by` places (via negative values,
/// so the per-session unique `seq` holds at every step).
fn make_room(tx: &Transaction<'_>, session: SessionId, after: i64, by: i64) -> Result<(), Error> {
    let id = session.to_string();
    tx.execute(
        "UPDATE messages SET seq = -(seq + ?3) WHERE session_id = ?1 AND seq > ?2",
        params![id, after, by],
    )
    .map_err(sql)?;
    tx.execute(
        "UPDATE messages SET seq = -seq WHERE session_id = ?1 AND seq < 0",
        [id],
    )
    .map_err(sql)?;
    Ok(())
}

impl Store {
    /// Commits `compaction` on the session `lock` covers.
    ///
    /// # Errors
    ///
    /// `store.lease_lost` (not a held compaction lock),
    /// `store.compaction_conflict` (a read row changed; nothing changed),
    /// `store.constraint`, `store.io`, `store.schema_too_new`,
    /// `store.sqlite`.
    pub fn compact(&self, lock: &Lease, compaction: &Compaction) -> Result<Compacted, Error> {
        check_shape(lock, compaction)?;
        self.ensure_writable()?;
        let session = lock.session();
        let blob_dir = self.location().blobs();
        let rows = compaction
            .replacements
            .iter()
            .map(|m| messages::prepare(blob_dir, m))
            .collect::<Result<Vec<_>, _>>()?;
        self.write(|tx| {
            leases::check_held(tx, lock)?;
            verify_reads(tx, session, &compaction.read)?;
            let epoch: u32 = tx
                .query_row(
                    "SELECT compaction_count + 1 FROM sessions WHERE id = ?1",
                    [session.to_string()],
                    |row| row.get(0),
                )
                .map_err(sql)?;
            let ids: Vec<String> = compaction.replaced.iter().map(ToString::to_string).collect();
            let mut last: i64 = 0;
            for id in &ids {
                let seq: i64 = tx
                    .query_row("SELECT seq FROM messages WHERE id = ?1", [id], |row| row.get(0))
                    .map_err(sql)?;
                last = last.max(seq);
                tx.execute(
                    "UPDATE messages SET active = 0, compacted = 1, compaction_epoch = ?2 WHERE id = ?1",
                    params![id, epoch],
                )
                .map_err(sql)?;
            }
            let count = i64::try_from(rows.len()).unwrap_or(i64::MAX);
            make_room(tx, session, last, count)?;
            let inserted = messages::insert(tx, session, &rows, last + 1)?;
            tx.execute(
                "UPDATE sessions SET compaction_count = ?2 WHERE id = ?1",
                params![session.to_string(), epoch],
            )
            .map_err(sql)?;
            Ok(Compacted { epoch, inserted })
        })
    }
}
