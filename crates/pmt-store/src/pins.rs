//! A session's pinned messages (spec: context-compression, Pinned messages
//! survive compaction).
//!
//! The text is captured when the message is pinned: compaction archives
//! the row, but the pin keeps its words for every later summary.

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_core::id::{MessageId, SessionId};
use rusqlite::params;

use crate::error::{corrupt, sql};
use crate::store::Store;

/// One pinned message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    /// The pinned message.
    pub message_id: MessageId,
    /// Its text, verbatim.
    pub text: String,
    /// When it was pinned.
    pub pinned_at: Timestamp,
}

impl Store {
    /// Pins `message` of `session` with `text` (pinning it again keeps
    /// its first pin time).
    ///
    /// # Errors
    ///
    /// `store.sqlite` (also for an unknown session).
    pub fn pin(&self, session: SessionId, message: MessageId, text: &str) -> Result<(), Error> {
        let now = Timestamp::now().as_millisecond();
        self.write(|tx| {
            tx.execute(
                "INSERT INTO session_pins (session_id, message_id, text, pinned_at) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (session_id, message_id) DO UPDATE SET text = excluded.text",
                params![session.to_string(), message.to_string(), text, now],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// Unpins `message`; whether it was pinned.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn unpin(&self, session: SessionId, message: MessageId) -> Result<bool, Error> {
        self.write(|tx| {
            tx.execute(
                "DELETE FROM session_pins WHERE session_id = ?1 AND message_id = ?2",
                params![session.to_string(), message.to_string()],
            )
            .map(|n| n > 0)
            .map_err(sql)
        })
    }

    /// The pins of `session`, oldest first.
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.corrupt`.
    pub fn pins(&self, session: SessionId) -> Result<Vec<Pin>, Error> {
        let rows: Vec<(String, String, i64)> = self.read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT message_id, text, pinned_at FROM session_pins \
                     WHERE session_id = ?1 ORDER BY rowid",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([session.to_string()], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .map_err(sql)?;
            rows.collect::<Result<_, _>>().map_err(sql)
        })?;
        rows.into_iter()
            .map(|(id, text, at)| {
                Ok(Pin {
                    message_id: MessageId::parse(&id)
                        .map_err(|e| corrupt("session_pins.message_id", e))?,
                    text,
                    pinned_at: Timestamp::from_millisecond(at)
                        .map_err(|e| corrupt("session_pins.pinned_at", e))?,
                })
            })
            .collect()
    }
}
