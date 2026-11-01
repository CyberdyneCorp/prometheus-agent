//! Rewind as a soft delete (spec: session-storage, Rewind is a soft
//! delete).
//!
//! Rewinding to a user message deactivates it and every later active
//! message of the session (`active = 0`, never deleted) and tags them with
//! the rewind's batch number (the session's new `rewind_count`), so the
//! model no longer sees them while they stay in the database and one rewind
//! can be restored as a unit.

use pmt_core::error::{Error, codes};
use pmt_core::id::{MessageId, SessionId};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::contention::WriteClass;
use crate::error::sql;
use crate::leases::{self, Lease, LeaseKind};
use crate::sessions;
use crate::store::Store;

/// A committed rewind or restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewound {
    /// The rewind batch (the session's `rewind_count` after the rewind).
    pub batch: u32,
    /// The messages deactivated (or reactivated), in `seq` order.
    pub messages: Vec<MessageId>,
}

struct Target {
    seq: i64,
    batch: Option<u32>,
}

fn invalid(session: SessionId, target: MessageId, why: &str) -> Error {
    Error::new(
        &codes::STORE_REWIND_TARGET_INVALID,
        format!("cannot rewind session {session} to {target}: {why}"),
    )
}

fn conflict(why: &str) -> Error {
    Error::new(
        &codes::STORE_REWIND_CONFLICT,
        format!("{why}; nothing changed"),
    )
}

/// The target row: it must belong to `session` and be a user message that
/// is active (for a rewind) or rewound (for a restore).
fn target(
    conn: &Connection,
    session: SessionId,
    id: MessageId,
    rewound: bool,
) -> Result<Target, Error> {
    let row = conn
        .query_row(
            "SELECT session_id, role, active, compacted, seq, rewind_batch FROM messages WHERE id = ?1",
            [id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<u32>>(5)?,
                ))
            },
        )
        .optional()
        .map_err(sql)?;
    let Some((owner, role, active, compacted, seq, batch)) = row else {
        return Err(invalid(session, id, "no such message"));
    };
    if owner != session.to_string() {
        return Err(invalid(
            session,
            id,
            "the message belongs to another session",
        ));
    }
    if role != "user" {
        return Err(invalid(session, id, "not a user message"));
    }
    let state_ok = if rewound {
        !active && !compacted && batch.is_some()
    } else {
        active
    };
    if !state_ok {
        let why = if rewound {
            "the message was not rewound"
        } else {
            "the message is not active"
        };
        return Err(invalid(session, id, why));
    }
    Ok(Target { seq, batch })
}

fn ids(
    tx: &Transaction<'_>,
    sql_text: &str,
    session: SessionId,
    n: i64,
) -> Result<Vec<MessageId>, Error> {
    let mut stmt = tx.prepare(sql_text).map_err(sql)?;
    let rows = stmt
        .query_map(params![session.to_string(), n], |row| {
            row.get::<_, String>(0)
        })
        .map_err(sql)?;
    rows.map(|id| MessageId::parse(&id.map_err(sql)?)).collect()
}

fn ensure_turn_free(
    tx: &Transaction<'_>,
    session: SessionId,
    own: Option<&Lease>,
) -> Result<(), Error> {
    match leases::held_by_other(tx, session, LeaseKind::Turn, own)? {
        Some(holder) => Err(leases::in_use(session, LeaseKind::Turn, &holder)),
        None => Ok(()),
    }
}

impl Store {
    /// Rewinds `session` to the user message `target`: it and every later
    /// active message become inactive. `expected`, when given, must be
    /// exactly the messages that will be deactivated (checked inside the
    /// transaction). `own` is the caller's turn lease, if it holds one.
    ///
    /// # Errors
    ///
    /// `store.rewind_target_invalid` (missing, inactive or not a user
    /// message), `store.rewind_conflict` (`expected` differs),
    /// `session.in_use` (another holder owns the turn lease),
    /// `session.not_found`, `store.busy`, `store.sqlite`.
    pub fn rewind(
        &self,
        session: SessionId,
        target: MessageId,
        expected: Option<&[MessageId]>,
        own: Option<&Lease>,
    ) -> Result<Rewound, Error> {
        self.write_as(WriteClass::Transcript, |tx| {
            let current = sessions::load(tx, session)?.ok_or_else(|| sessions::not_found(session))?;
            ensure_turn_free(tx, session, own)?;
            let found = self::target(tx, session, target, false)?;
            let messages = ids(
                tx,
                "SELECT id FROM messages WHERE session_id = ?1 AND active = 1 AND seq >= ?2 ORDER BY seq",
                session,
                found.seq,
            )?;
            if expected.is_some_and(|expected| expected != messages.as_slice()) {
                return Err(conflict("the transcript changed since the rewind was planned"));
            }
            let batch = current.rewind_count + 1;
            tx.execute(
                "UPDATE messages SET active = 0, rewind_batch = ?3 \
                 WHERE session_id = ?1 AND active = 1 AND seq >= ?2",
                params![session.to_string(), found.seq, batch],
            )
            .map_err(sql)?;
            tx.execute(
                "UPDATE sessions SET rewind_count = ?2, last_activity_at = ?3 WHERE id = ?1",
                params![session.to_string(), batch, sessions::now_ms()],
            )
            .map_err(sql)?;
            Ok(Rewound { batch, messages })
        })
    }

    /// `/undo`: rewinds to the session's last active user message.
    ///
    /// # Errors
    ///
    /// `store.rewind_target_invalid` when there is no active user message,
    /// otherwise as [`Store::rewind`].
    pub fn undo_last_turn(
        &self,
        session: SessionId,
        own: Option<&Lease>,
    ) -> Result<Rewound, Error> {
        let last: Option<String> = self.read(|conn| {
            conn.query_row(
                "SELECT id FROM messages WHERE session_id = ?1 AND active = 1 AND role = 'user' \
                 ORDER BY seq DESC LIMIT 1",
                [session.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)
        })?;
        let Some(last) = last else {
            return Err(Error::new(
                &codes::STORE_REWIND_TARGET_INVALID,
                format!("session {session} has no active user message to rewind to"),
            ));
        };
        self.rewind(session, MessageId::parse(&last)?, None, own)
    }

    /// Restores the rewind that deactivated `message` (reactivating its
    /// whole batch), provided nothing was appended after it since.
    ///
    /// # Errors
    ///
    /// `store.rewind_target_invalid` (not a rewound user message),
    /// `store.rewind_conflict` (later messages are active),
    /// `session.in_use`, `session.not_found`, `store.busy`, `store.sqlite`.
    pub fn restore_rewind(
        &self,
        session: SessionId,
        message: MessageId,
        own: Option<&Lease>,
    ) -> Result<Rewound, Error> {
        self.write_as(WriteClass::Transcript, |tx| {
            sessions::load(tx, session)?.ok_or_else(|| sessions::not_found(session))?;
            ensure_turn_free(tx, session, own)?;
            let found = self::target(tx, session, message, true)?;
            let batch = found.batch.unwrap_or_default();
            let later: i64 = tx
                .query_row(
                    "SELECT count(*) FROM messages WHERE session_id = ?1 AND active = 1 AND seq > ?2",
                    params![session.to_string(), found.seq],
                    |row| row.get(0),
                )
                .map_err(sql)?;
            if later > 0 {
                return Err(conflict("messages were added after the rewind"));
            }
            let messages = ids(
                tx,
                "SELECT id FROM messages WHERE session_id = ?1 AND rewind_batch = ?2 ORDER BY seq",
                session,
                i64::from(batch),
            )?;
            tx.execute(
                "UPDATE messages SET active = 1, rewind_batch = NULL \
                 WHERE session_id = ?1 AND rewind_batch = ?2",
                params![session.to_string(), batch],
            )
            .map_err(sql)?;
            Ok(Rewound { batch, messages })
        })
    }
}
