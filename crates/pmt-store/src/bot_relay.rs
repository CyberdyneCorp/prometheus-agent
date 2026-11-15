//! The desktop relay's durable state (spec: agent-interop, Desktop relay
//! for cross-connection bot messages).
//!
//! The sending profile keeps an outbox of signed envelopes. A courier (a
//! desktop holding both connections) drains them; an envelope drained but
//! not settled is offered once more after the courier's delivery deadline,
//! and fails with `delivery_timeout` once twice that deadline passed since
//! its first offer. An envelope still queued (never drained) when it is
//! older than the TTL fails with `queued_expired` instead of being
//! offered. The target profile keeps an inbox of delivery ids so a
//! delivery never runs twice, however often a courier retries it.

use jiff::Timestamp;
use pmt_core::error::Error;
use rusqlite::{OptionalExtension as _, params};

use crate::error::{corrupt, sql};
use crate::store::Store;

/// A queued envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// The delivery id (never runs twice on the target).
    pub delivery_id: String,
    /// The sender's handle.
    pub sender: String,
    /// The target's handle (`<handle>@<connection>`).
    pub target: String,
    /// The message.
    pub message: String,
    /// The sender's signature over the fields above.
    pub signature: String,
    /// When it was queued.
    pub created_at: Timestamp,
}

/// Where an envelope is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayState {
    /// Not drained yet.
    Queued,
    /// Drained `offers` times, not settled.
    Offered {
        /// Times offered (1 or 2).
        offers: u32,
    },
    /// The target replied.
    Delivered {
        /// The reply.
        reply: String,
    },
    /// It failed.
    Failed {
        /// `queued_expired`, `delivery_timeout`, or the target's reason.
        reason: String,
    },
}

/// The relay's timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayTiming {
    /// How long a courier has to deliver a drained envelope before it is
    /// offered again.
    pub deadline_ms: i64,
    /// Envelopes still queued after this long fail (`0`: never).
    pub ttl_ms: i64,
}

/// What a drain found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Drained {
    /// Envelopes for the courier.
    pub offered: Vec<Envelope>,
    /// Envelopes failed as `queued_expired` instead.
    pub expired: Vec<String>,
}

/// A relayed delivery this profile received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    /// The session it runs in.
    pub session_id: String,
    /// `running`, `delivered` or `failed`.
    pub state: String,
    /// The reply, once delivered.
    pub reply: Option<String>,
    /// The reason, once failed.
    pub reason: Option<String>,
}

type OutboxRow = (String, String, String, String, String, i64);

fn envelope(row: OutboxRow) -> Result<Envelope, Error> {
    let (delivery_id, sender, target, message, signature, created) = row;
    Ok(Envelope {
        delivery_id,
        sender,
        target,
        message,
        signature,
        created_at: Timestamp::from_millisecond(created)
            .map_err(|e| corrupt("bot_relay_outbox.created_at", e))?,
    })
}

impl Store {
    /// Queues `envelope` (queuing the same delivery id again is a no-op).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn relay_enqueue(&self, envelope: &Envelope) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO bot_relay_outbox (delivery_id, sender, target, message, \
                 signature, created_at, state) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'queued')",
                params![
                    envelope.delivery_id,
                    envelope.sender,
                    envelope.target,
                    envelope.message,
                    envelope.signature,
                    envelope.created_at.as_millisecond()
                ],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// Hands out what a courier may deliver at `now`: queued envelopes
    /// (or fails them as `queued_expired` past the TTL), and envelopes
    /// offered once whose deadline passed (offered a second, last time).
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.corrupt`.
    pub fn relay_drain(&self, now: Timestamp, timing: RelayTiming) -> Result<Drained, Error> {
        let now = now.as_millisecond();
        let rows: Vec<(OutboxRow, i64)> = self.write(|tx| {
            let mut stmt = tx
                .prepare(
                    "SELECT delivery_id, sender, target, message, signature, created_at, offers \
                     FROM bot_relay_outbox WHERE state = 'queued' \
                     OR (state = 'offered' AND offers = 1 AND offered_at <= ?1) \
                     ORDER BY created_at, delivery_id",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([now - timing.deadline_ms], |r| {
                    Ok((
                        (
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                        ),
                        r.get(6)?,
                    ))
                })
                .map_err(sql)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql)?;
            for (row, offers) in &rows {
                let queued_too_long =
                    *offers == 0 && timing.ttl_ms > 0 && now - row.5 > timing.ttl_ms;
                if queued_too_long {
                    tx.execute(
                        "UPDATE bot_relay_outbox SET state = 'failed', reason = 'queued_expired', \
                         settled_at = ?2 WHERE delivery_id = ?1",
                        params![row.0, now],
                    )
                    .map_err(sql)?;
                } else {
                    tx.execute(
                        "UPDATE bot_relay_outbox SET state = 'offered', offers = offers + 1, \
                         offered_at = ?2, first_offer = COALESCE(first_offer, ?2) \
                         WHERE delivery_id = ?1",
                        params![row.0, now],
                    )
                    .map_err(sql)?;
                }
            }
            Ok(rows)
        })?;
        let mut drained = Drained::default();
        for (row, offers) in rows {
            let expired = offers == 0 && timing.ttl_ms > 0 && now - row.5 > timing.ttl_ms;
            if expired {
                drained.expired.push(row.0);
            } else {
                drained.offered.push(envelope(row)?);
            }
        }
        Ok(drained)
    }

    /// Fails as `delivery_timeout` every offered envelope whose first
    /// offer is at least twice the deadline old at `now`; their ids.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn relay_time_out(
        &self,
        now: Timestamp,
        timing: RelayTiming,
    ) -> Result<Vec<String>, Error> {
        let now = now.as_millisecond();
        self.write(|tx| {
            let ids: Vec<String> = tx
                .prepare(
                    "SELECT delivery_id FROM bot_relay_outbox \
                     WHERE state = 'offered' AND first_offer <= ?1",
                )
                .map_err(sql)?
                .query_map([now - 2 * timing.deadline_ms], |r| r.get(0))
                .map_err(sql)?
                .collect::<Result<_, _>>()
                .map_err(sql)?;
            for id in &ids {
                tx.execute(
                    "UPDATE bot_relay_outbox SET state = 'failed', reason = 'delivery_timeout', \
                     settled_at = ?2 WHERE delivery_id = ?1",
                    params![id, now],
                )
                .map_err(sql)?;
            }
            Ok(ids)
        })
    }

    /// Settles delivery `id` whose signature is `signature` with the
    /// target's reply (`Ok`) or reason (`Err`); `false` when no unsettled
    /// envelope has that id and signature (a forged or late settle).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn relay_settle(
        &self,
        id: &str,
        signature: &str,
        outcome: Result<&str, &str>,
    ) -> Result<bool, Error> {
        let now = Timestamp::now().as_millisecond();
        let (state, reply, reason) = match outcome {
            Ok(reply) => ("delivered", Some(reply), None),
            Err(reason) => ("failed", None, Some(reason)),
        };
        self.write(|tx| {
            tx.execute(
                "UPDATE bot_relay_outbox SET state = ?3, reply = ?4, reason = ?5, settled_at = ?6 \
                 WHERE delivery_id = ?1 AND signature = ?2 AND state IN ('queued', 'offered')",
                params![id, signature, state, reply, reason, now],
            )
            .map(|n| n > 0)
            .map_err(sql)
        })
    }

    /// The state of delivery `id` in the outbox.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn relay_state(&self, id: &str) -> Result<Option<RelayState>, Error> {
        type Row = (String, u32, Option<String>, Option<String>);
        let row: Option<Row> = self.read(|conn| {
            conn.query_row(
                "SELECT state, offers, reply, reason FROM bot_relay_outbox WHERE delivery_id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(sql)
        })?;
        Ok(
            row.map(|(state, offers, reply, reason)| match state.as_str() {
                "queued" => RelayState::Queued,
                "offered" => RelayState::Offered { offers },
                "delivered" => RelayState::Delivered {
                    reply: reply.unwrap_or_default(),
                },
                _ => RelayState::Failed {
                    reason: reason.unwrap_or_else(|| "unknown".to_owned()),
                },
            }),
        )
    }

    /// Records relayed delivery `id` as received into `session`; the
    /// existing record when it was received before (then nothing runs).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn relay_receive(&self, id: &str, session: &str) -> Result<Option<Received>, Error> {
        let now = Timestamp::now().as_millisecond();
        self.write(|tx| {
            let existing = tx
                .query_row(
                    "SELECT session_id, state, reply, reason FROM bot_relay_inbox \
                     WHERE delivery_id = ?1",
                    [id],
                    |r| {
                        Ok(Received {
                            session_id: r.get(0)?,
                            state: r.get(1)?,
                            reply: r.get(2)?,
                            reason: r.get(3)?,
                        })
                    },
                )
                .optional()
                .map_err(sql)?;
            if existing.is_none() {
                tx.execute(
                    "INSERT INTO bot_relay_inbox (delivery_id, session_id, state, received_at) \
                     VALUES (?1, ?2, 'running', ?3)",
                    params![id, session, now],
                )
                .map_err(sql)?;
            }
            Ok(existing)
        })
    }

    /// Records how received delivery `id` ended.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn relay_received(&self, id: &str, outcome: Result<&str, &str>) -> Result<(), Error> {
        let (state, reply, reason) = match outcome {
            Ok(reply) => ("delivered", Some(reply), None),
            Err(reason) => ("failed", None, Some(reason)),
        };
        self.write(|tx| {
            tx.execute(
                "UPDATE bot_relay_inbox SET state = ?2, reply = ?3, reason = ?4 \
                 WHERE delivery_id = ?1",
                params![id, state, reply, reason],
            )
            .map(drop)
            .map_err(sql)
        })
    }
}
