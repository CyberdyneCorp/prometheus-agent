//! The durable delivery outbox (spec: messaging-gateway, Durable delivery
//! outbox): every outbound message is stored with an idempotency key
//! before its first send attempt and moves `queued` -> `sending` -> `sent`
//! (then optionally `delivered`, `read`), or to `failed` or `unknown`.
//! Entries of one chat leave in order: an entry is due only when no older
//! entry of its chat is still queued or sending.

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use rusqlite::{OptionalExtension, Row, params};

use super::{millis, timestamp};
use crate::error::{corrupt, sql};
use crate::store::Store;

/// What an entry carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundKind {
    /// (Part of) a turn's answer.
    Reply,
    /// A background or system notification.
    Notification,
    /// An approval card.
    Approval,
    /// A gateway message (pairing codes, busy acknowledgements, notices).
    System,
}

impl OutboundKind {
    /// The stored value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::Notification => "notification",
            Self::Approval => "approval",
            Self::System => "system",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "reply" => Self::Reply,
            "notification" => Self::Notification,
            "approval" => Self::Approval,
            "system" => Self::System,
            _ => return None,
        })
    }
}

/// Where an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundState {
    /// Waiting for its (next) attempt.
    Queued,
    /// An attempt is in flight.
    Sending,
    /// The platform accepted it.
    Sent,
    /// The platform reported delivery.
    Delivered,
    /// The platform reported it read.
    Read,
    /// Given up.
    Failed,
    /// An attempt's outcome is unknown; never resent automatically.
    Unknown,
}

impl OutboundState {
    /// The stored value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Sending => "sending",
            Self::Sent => "sent",
            Self::Delivered => "delivered",
            Self::Read => "read",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    /// Parses a stored or typed state.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "queued" => Self::Queued,
            "sending" => Self::Sending,
            "sent" => Self::Sent,
            "delivered" => Self::Delivered,
            "read" => Self::Read,
            "failed" => Self::Failed,
            "unknown" => Self::Unknown,
            _ => return None,
        })
    }
}

/// A file sent with a message.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OutboundAttachment {
    /// Where the daemon reads it.
    pub path: String,
    /// Sent as a document (images uncompressed).
    #[serde(default)]
    pub as_document: bool,
    /// Sent as a voice note.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub as_voice: bool,
}

impl OutboundAttachment {
    /// A file at `path`, sent as the platform sends its kind.
    #[must_use]
    pub fn new(path: &str) -> Self {
        Self {
            path: path.to_owned(),
            as_document: false,
            as_voice: false,
        }
    }
}

/// An entry to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOutbound {
    /// `<session_id>:<turn_id>:<part>` (or another unique key for
    /// messages outside a turn).
    pub idempotency_key: String,
    /// The session it belongs to.
    pub session_id: Option<String>,
    /// Platform name.
    pub platform: String,
    /// Chat id.
    pub chat_id: String,
    /// Thread id.
    pub thread_id: Option<String>,
    /// Platform message id it answers.
    pub reply_to: Option<String>,
    /// Platform message id it replaces (edited in place where the
    /// platform can; sent as a new message otherwise).
    pub edit_of: Option<String>,
    /// What it carries.
    pub kind: OutboundKind,
    /// The text (an approval card's JSON for `approval`).
    pub body: String,
    /// Files sent with it.
    pub attachments: Vec<OutboundAttachment>,
}

/// A stored entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outbound {
    /// Row id.
    pub id: i64,
    /// The idempotency key.
    pub idempotency_key: String,
    /// The session it belongs to.
    pub session_id: Option<String>,
    /// Platform name.
    pub platform: String,
    /// Chat id.
    pub chat_id: String,
    /// Thread id.
    pub thread_id: Option<String>,
    /// Platform message id it answers.
    pub reply_to: Option<String>,
    /// Platform message id it replaces.
    pub edit_of: Option<String>,
    /// What it carries.
    pub kind: OutboundKind,
    /// The text.
    pub body: String,
    /// Files sent with it.
    pub attachments: Vec<OutboundAttachment>,
    /// Where it is.
    pub state: OutboundState,
    /// Attempts that counted against the limit.
    pub attempts: u32,
    /// When it is due next.
    pub next_attempt_at: Timestamp,
    /// When it was stored.
    pub created_at: Timestamp,
    /// Last state change.
    pub updated_at: Timestamp,
    /// The platform's id of the sent message.
    pub platform_message_id: Option<String>,
    /// The last error.
    pub last_error: Option<String>,
}

const COLUMNS: &str = "id, idempotency_key, session_id, platform, chat_id, thread_id, reply_to, \
    edit_of, kind, body, state, attempts, next_attempt_at, created_at, updated_at, platform_message_id, \
    last_error, attachments";

fn from_row(row: &Row<'_>) -> Result<Outbound, Error> {
    let kind: String = row.get(8).map_err(sql)?;
    let state: String = row.get(10).map_err(sql)?;
    Ok(Outbound {
        id: row.get(0).map_err(sql)?,
        idempotency_key: row.get(1).map_err(sql)?,
        session_id: row.get(2).map_err(sql)?,
        platform: row.get(3).map_err(sql)?,
        chat_id: row.get(4).map_err(sql)?,
        thread_id: row.get(5).map_err(sql)?,
        reply_to: row.get(6).map_err(sql)?,
        edit_of: row.get(7).map_err(sql)?,
        kind: OutboundKind::parse(&kind).ok_or_else(|| corrupt("gateway_outbox.kind", &kind))?,
        body: row.get(9).map_err(sql)?,
        state: OutboundState::parse(&state)
            .ok_or_else(|| corrupt("gateway_outbox.state", &state))?,
        attempts: u32::try_from(row.get::<_, i64>(11).map_err(sql)?)
            .map_err(|e| corrupt("gateway_outbox.attempts", e))?,
        next_attempt_at: timestamp(row.get(12).map_err(sql)?),
        created_at: timestamp(row.get(13).map_err(sql)?),
        updated_at: timestamp(row.get(14).map_err(sql)?),
        platform_message_id: row.get(15).map_err(sql)?,
        last_error: row.get(16).map_err(sql)?,
        attachments: match row.get::<_, Option<String>>(17).map_err(sql)? {
            Some(text) => {
                serde_json::from_str(&text).map_err(|e| corrupt("gateway_outbox.attachments", e))?
            }
            None => Vec::new(),
        },
    })
}

fn query(
    conn: &rusqlite::Connection,
    sql_text: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<Outbound>, Error> {
    let mut stmt = conn.prepare(sql_text).map_err(sql)?;
    let rows = stmt
        .query_map(params, |row| Ok(from_row(row)))
        .map_err(sql)?;
    let mut entries = Vec::new();
    for row in rows {
        entries.push(row.map_err(sql)??);
    }
    Ok(entries)
}

fn entry_error(id: i64, what: &str) -> Error {
    Error::new(
        &codes::GATEWAY_OUTBOX_ENTRY,
        format!("outbox entry {id} {what}"),
    )
}

impl Store {
    /// Stores an entry, queued and due now. Storing a key again returns
    /// the existing entry unchanged (`false`: not new).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn enqueue_outbound(&self, new: &NewOutbound) -> Result<(Outbound, bool), Error> {
        let now = millis(Timestamp::now());
        let attachments = if new.attachments.is_empty() {
            None
        } else {
            Some(
                serde_json::to_string(&new.attachments)
                    .map_err(|e| corrupt("gateway_outbox.attachments", e))?,
            )
        };
        self.write(|tx| {
            let inserted = tx
                .execute(
                    "INSERT INTO gateway_outbox (idempotency_key, session_id, platform, chat_id, \
                     thread_id, reply_to, edit_of, kind, body, state, next_attempt_at, \
                     created_at, updated_at, attachments) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, \
                     ?9, 'queued', ?10, ?10, ?10, ?11) ON CONFLICT (idempotency_key) DO NOTHING",
                    params![
                        new.idempotency_key,
                        new.session_id,
                        new.platform,
                        new.chat_id,
                        new.thread_id,
                        new.reply_to,
                        new.edit_of,
                        new.kind.as_str(),
                        new.body,
                        now,
                        attachments
                    ],
                )
                .map_err(sql)?;
            let entry = query(
                tx,
                &format!("SELECT {COLUMNS} FROM gateway_outbox WHERE idempotency_key = ?1"),
                [&new.idempotency_key],
            )?
            .pop()
            .ok_or_else(|| corrupt("gateway_outbox", "an entry vanished while stored"))?;
            Ok((entry, inserted == 1))
        })
    }

    /// Queued entries due at `now` that head their chat's queue, oldest
    /// first.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn due_outbound(&self, now: Timestamp, limit: usize) -> Result<Vec<Outbound>, Error> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.read(|conn| {
            query(
                conn,
                &format!(
                    "SELECT {COLUMNS} FROM gateway_outbox AS o WHERE state = 'queued' \
                     AND next_attempt_at <= ?1 AND NOT EXISTS (SELECT 1 FROM gateway_outbox AS e \
                     WHERE e.platform = o.platform AND e.chat_id = o.chat_id AND e.id < o.id \
                     AND e.state IN ('queued', 'sending')) ORDER BY id LIMIT ?2"
                ),
                params![millis(now), limit],
            )
        })
    }

    /// The entry `id`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn outbound(&self, id: i64) -> Result<Option<Outbound>, Error> {
        self.read(|conn| {
            Ok(query(
                conn,
                &format!("SELECT {COLUMNS} FROM gateway_outbox WHERE id = ?1"),
                [id],
            )?
            .pop())
        })
    }

    /// Entries, newest first, optionally only those in `state`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn outbox_entries(
        &self,
        state: Option<OutboundState>,
        limit: usize,
    ) -> Result<Vec<Outbound>, Error> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.read(|conn| {
            query(
                conn,
                &format!(
                    "SELECT {COLUMNS} FROM gateway_outbox WHERE (?1 IS NULL OR state = ?1) \
                     ORDER BY id DESC LIMIT ?2"
                ),
                params![state.map(OutboundState::as_str), limit],
            )
        })
    }

    /// How many entries are in `state`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn outbox_count(&self, state: OutboundState) -> Result<u64, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM gateway_outbox WHERE state = ?1",
                [state.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| u64::try_from(n).unwrap_or(0))
            .map_err(sql)
        })
    }

    /// Moves a queued entry to `sending` (an attempt starts); `false`
    /// when it is no longer queued.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn start_sending(&self, id: i64) -> Result<bool, Error> {
        self.set_state(id, "state = 'queued'", "state = 'sending'", params![])
    }

    /// The attempt succeeded.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn sent(&self, id: i64, platform_message_id: Option<&str>) -> Result<(), Error> {
        self.set_state(
            id,
            "state = 'sending'",
            "state = 'sent', platform_message_id = ?3, last_error = NULL",
            params![platform_message_id],
        )
        .map(drop)
    }

    /// The attempt failed transiently: queued again for `next`, counting
    /// against the attempt limit when `counted`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn retry_later(
        &self,
        id: i64,
        next: Timestamp,
        counted: bool,
        error: &str,
    ) -> Result<(), Error> {
        self.set_state(
            id,
            "state = 'sending'",
            "state = 'queued', attempts = attempts + ?3, next_attempt_at = ?4, last_error = ?5",
            params![i64::from(counted), millis(next), error],
        )
        .map(drop)
    }

    /// The entry is given up (`failed`) or its outcome is unknown.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn settle_outbound(&self, id: i64, state: OutboundState, error: &str) -> Result<(), Error> {
        self.set_state(
            id,
            "state IN ('queued', 'sending')",
            "state = ?3, last_error = ?4",
            params![state.as_str(), error],
        )
        .map(drop)
    }

    /// Startup after a crash: an attempt that was in flight has an
    /// unknown outcome. Returns how many entries became `unknown`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn abandon_in_flight(&self) -> Result<usize, Error> {
        let now = millis(Timestamp::now());
        self.write(|tx| {
            tx.execute(
                "UPDATE gateway_outbox SET state = 'unknown', updated_at = ?1, \
                 last_error = 'the daemon stopped during the send' WHERE state = 'sending'",
                [now],
            )
            .map_err(sql)
        })
    }

    /// `gateway outbox retry`: a `failed` or `unknown` entry is queued
    /// again, due now, with its attempts reset.
    ///
    /// # Errors
    ///
    /// `gateway.outbox_entry` for another state or no such entry,
    /// `store.sqlite`.
    pub fn requeue_outbound(&self, id: i64) -> Result<(), Error> {
        let now = millis(Timestamp::now());
        let changed = self.set_state(
            id,
            "state IN ('failed', 'unknown')",
            "state = 'queued', attempts = 0, next_attempt_at = ?3, last_error = NULL",
            params![now],
        )?;
        if changed {
            Ok(())
        } else {
            Err(entry_error(id, "is not failed or unknown"))
        }
    }

    /// `gateway outbox drop`: removes an entry that is not being sent.
    ///
    /// # Errors
    ///
    /// `gateway.outbox_entry`, `store.sqlite`.
    pub fn drop_outbound(&self, id: i64) -> Result<(), Error> {
        let removed = self.write(|tx| {
            tx.execute(
                "DELETE FROM gateway_outbox WHERE id = ?1 AND state != 'sending'",
                [id],
            )
            .map_err(sql)
        })?;
        if removed == 1 {
            Ok(())
        } else {
            Err(entry_error(id, "does not exist or is being sent"))
        }
    }

    /// Changes the entry when `guard` holds; `true` when it did. `extra`
    /// binds `?3` onwards.
    fn set_state(
        &self,
        id: i64,
        guard: &str,
        set: &str,
        extra: &[&dyn rusqlite::ToSql],
    ) -> Result<bool, Error> {
        let now = millis(Timestamp::now());
        self.write(|tx| {
            let mut bound: Vec<&dyn rusqlite::ToSql> = vec![&id, &now];
            bound.extend_from_slice(extra);
            let changed = tx
                .execute(
                    &format!(
                        "UPDATE gateway_outbox SET {set}, updated_at = ?2 WHERE id = ?1 AND {guard}"
                    ),
                    bound.as_slice(),
                )
                .map_err(sql)?;
            Ok(changed == 1)
        })
    }

    /// The latest platform message id stored for the idempotency key
    /// (edits of a card or a streamed draft target it).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn outbound_message_id(&self, key: &str) -> Result<Option<String>, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT platform_message_id FROM gateway_outbox WHERE idempotency_key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()
            .map(Option::flatten)
            .map_err(sql)
        })
    }
}
