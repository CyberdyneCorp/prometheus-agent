//! Bot-to-bot deliveries (spec: bot-mode, `message_agent` tool; Typed
//! delivery failures and safe retry).
//!
//! A sent message is an `out` row in the sender's store. When the target
//! is a profile of the same home its store gets the `in` row: the durable
//! ingress record a daemon runs as the target Bot Chat's next turn. Both
//! rows share the delivery id; claiming an `in` row moves it to `running`
//! in one write, so one delivery never runs twice.

use jiff::Timestamp;
use pmt_core::error::Error;
use rusqlite::{OptionalExtension as _, Row, params};

use crate::error::{corrupt, sql};
use crate::sessions::now_ms;
use crate::store::Store;

/// Which side of a delivery a row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// What this profile sent.
    Out,
    /// What this profile receives.
    In,
}

impl Direction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Out => "out",
            Self::In => "in",
        }
    }
}

/// One bot message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotMessage {
    /// Never runs twice.
    pub delivery_id: String,
    /// `local` (a profile of this home), `peer` or `connection`.
    pub route: String,
    /// The sending profile (or remote handle).
    pub sender: String,
    /// "🤖 <title> (@<handle>)": who the target is told sent it.
    pub attribution: String,
    /// The resolved target: a profile, `<peer>/<agent>` or
    /// `<handle>@<connection>`.
    pub target: String,
    /// The message.
    pub message: String,
    /// Bot-originated hops of the chain this message continues (1 for a
    /// message sent from a turn a person started).
    pub hops: u32,
    /// `out`: the sender's Bot Chat, which hears the outcome.
    pub origin_session: Option<String>,
    /// `in`: the target's Bot Chat the turn runs in.
    pub session_id: Option<String>,
    /// `queued`, `running`, `delivered` or `failed`.
    pub state: String,
    /// The failure reason code.
    pub reason: Option<String>,
    /// Redacted failure text.
    pub error: Option<String>,
    /// The target's reply (empty for an intentional silence).
    pub reply: Option<String>,
    /// Turns run for it (a retried delivery has two).
    pub attempts: u32,
    /// `out`: the sender's Bot Chat was told the outcome.
    pub notified: bool,
    /// When it was queued.
    pub created_at: Timestamp,
}

/// How a delivery ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The target answered.
    Delivered {
        /// The reply.
        reply: String,
    },
    /// It failed.
    Failed {
        /// The reason code.
        reason: String,
        /// Redacted error text.
        error: String,
    },
}

const COLUMNS: &str = "delivery_id, route, sender, attribution, target, message, hops, \
    origin_session, session_id, state, reason, error, reply, attempts, notified, created_at";

fn row(row: &Row<'_>) -> rusqlite::Result<(BotMessage, i64)> {
    Ok((
        BotMessage {
            delivery_id: row.get(0)?,
            route: row.get(1)?,
            sender: row.get(2)?,
            attribution: row.get(3)?,
            target: row.get(4)?,
            message: row.get(5)?,
            hops: row.get(6)?,
            origin_session: row.get(7)?,
            session_id: row.get(8)?,
            state: row.get(9)?,
            reason: row.get(10)?,
            error: row.get(11)?,
            reply: row.get(12)?,
            attempts: row.get(13)?,
            notified: row.get::<_, i64>(14)? != 0,
            created_at: Timestamp::UNIX_EPOCH,
        },
        row.get(15)?,
    ))
}

fn finish((mut message, created): (BotMessage, i64)) -> Result<BotMessage, Error> {
    message.created_at =
        Timestamp::from_millisecond(created).map_err(|e| corrupt("bot_messages.created_at", e))?;
    Ok(message)
}

impl Store {
    /// Records `message` on `direction`'s side (recording the same
    /// delivery id again is a no-op).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn bot_message_put(&self, direction: Direction, message: &BotMessage) -> Result<(), Error> {
        let now = now_ms();
        self.write(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO bot_messages (delivery_id, direction, route, sender, \
                 attribution, target, message, hops, origin_session, session_id, state, \
                 attempts, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0, ?12, ?12)",
                params![
                    message.delivery_id,
                    direction.as_str(),
                    message.route,
                    message.sender,
                    message.attribution,
                    message.target,
                    message.message,
                    message.hops,
                    message.origin_session,
                    message.session_id,
                    message.state,
                    message.created_at.as_millisecond().min(now),
                ],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// The `direction` row of delivery `id`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.corrupt`.
    pub fn bot_message(&self, direction: Direction, id: &str) -> Result<Option<BotMessage>, Error> {
        let found = self.read(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {COLUMNS} FROM bot_messages WHERE delivery_id = ?1 AND direction = ?2"
                ),
                params![id, direction.as_str()],
                row,
            )
            .optional()
            .map_err(sql)
        })?;
        found.map(finish).transpose()
    }

    /// The `direction` rows in `state`, oldest first.
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.corrupt`.
    pub fn bot_messages_in(
        &self,
        direction: Direction,
        state: &str,
    ) -> Result<Vec<BotMessage>, Error> {
        let rows = self.read(|conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM bot_messages WHERE direction = ?1 AND state = ?2 \
                     ORDER BY created_at, delivery_id"
                ))
                .map_err(sql)?;
            stmt.query_map(params![direction.as_str(), state], row)
                .map_err(sql)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql)
        })?;
        rows.into_iter().map(finish).collect()
    }

    /// Settled `out` rows whose sender was not told yet, oldest first.
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.corrupt`.
    pub fn bot_messages_unnotified(&self) -> Result<Vec<BotMessage>, Error> {
        let rows = self.read(|conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM bot_messages WHERE direction = 'out' AND notified = 0 \
                     AND state IN ('delivered', 'failed') ORDER BY updated_at, delivery_id"
                ))
                .map_err(sql)?;
            stmt.query_map([], row)
                .map_err(sql)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql)
        })?;
        rows.into_iter().map(finish).collect()
    }

    /// Moves the queued `in` row of `id` to `running` in `session`; `false`
    /// when it was not queued (another driver claimed it, or it ended).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn bot_message_claim(&self, id: &str, session: &str) -> Result<bool, Error> {
        let now = now_ms();
        self.write(|tx| {
            let changed = tx
                .execute(
                    "UPDATE bot_messages SET state = 'running', session_id = ?2, \
                     attempts = attempts + 1, updated_at = ?3 \
                     WHERE delivery_id = ?1 AND direction = 'in' AND state = 'queued'",
                    params![id, session, now],
                )
                .map_err(sql)?;
            Ok(changed == 1)
        })
    }

    /// Counts one more turn of a running `in` row (a retry).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn bot_message_retry(&self, id: &str) -> Result<(), Error> {
        let now = now_ms();
        self.write(|tx| {
            tx.execute(
                "UPDATE bot_messages SET attempts = attempts + 1, updated_at = ?2 \
                 WHERE delivery_id = ?1 AND direction = 'in' AND state = 'running'",
                params![id, now],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// Settles the `direction` row of `id` (an already settled row is left
    /// alone); `true` when this call settled it.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn bot_message_settle(
        &self,
        direction: Direction,
        id: &str,
        outcome: &Outcome,
    ) -> Result<bool, Error> {
        let now = now_ms();
        let (state, reply, reason, error) = match outcome {
            Outcome::Delivered { reply } => ("delivered", Some(reply.as_str()), None, None),
            Outcome::Failed { reason, error } => {
                ("failed", None, Some(reason.as_str()), Some(error.as_str()))
            }
        };
        self.write(|tx| {
            let changed = tx
                .execute(
                    "UPDATE bot_messages SET state = ?3, reply = ?4, reason = ?5, error = ?6, \
                     updated_at = ?7 WHERE delivery_id = ?1 AND direction = ?2 \
                     AND state IN ('queued', 'running')",
                    params![id, direction.as_str(), state, reply, reason, error, now],
                )
                .map_err(sql)?;
            Ok(changed == 1)
        })
    }

    /// Marks the `out` row of `id` as told to its sender.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn bot_message_notified(&self, id: &str) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "UPDATE bot_messages SET notified = 1 WHERE delivery_id = ?1 AND direction = 'out'",
                [id],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// The canonical Bot Chat (the newest live session with the
    /// `bot_chat` flag), without creating one.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn bot_chat_id(&self) -> Result<Option<String>, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT id FROM sessions WHERE bot_chat = 1 AND archived = 'none' \
                 ORDER BY started_at DESC, id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)
        })
    }

    /// The newest active user or assistant message of `session` with
    /// text: its text (`None` when spilled to a blob), time and position.
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.corrupt`.
    pub fn latest_text(
        &self,
        session: &str,
    ) -> Result<Option<(Option<String>, Timestamp, u64)>, Error> {
        let found: Option<(Option<String>, i64, i64)> = self.read(|conn| {
            conn.query_row(
                "SELECT content, created_at, seq FROM messages WHERE session_id = ?1 \
                 AND active = 1 AND role IN ('user', 'assistant') \
                 AND (content IS NOT NULL OR content_ref IS NOT NULL) \
                 ORDER BY seq DESC LIMIT 1",
                [session],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(sql)
        })?;
        found
            .map(|(text, at, seq)| {
                let at = Timestamp::from_millisecond(at)
                    .map_err(|e| corrupt("messages.created_at", e))?;
                Ok((text, at, u64::try_from(seq).unwrap_or(0)))
            })
            .transpose()
    }

    /// Active assistant messages of `session` after position `seq`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn replies_after(&self, session: &str, seq: u64) -> Result<u64, Error> {
        let seq = i64::try_from(seq).unwrap_or(i64::MAX);
        self.read(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND active = 1 \
                 AND role = 'assistant' AND content IS NOT NULL AND content != '' AND seq > ?2",
                params![session, seq],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| u64::try_from(n).unwrap_or(0))
            .map_err(sql)
        })
    }
}
