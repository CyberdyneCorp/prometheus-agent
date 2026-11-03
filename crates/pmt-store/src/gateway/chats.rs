//! What the gateway asks about chats beyond routing (spec: messaging-
//! gateway, Per-chat policies; Per-user rate limiting; agent-interop,
//! One-shot send command; `send_message` tool): the policy frozen for a
//! session, the chats the profile has sessions in, the session a chat
//! routes to now, and a sender's spend.

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::{OptionalExtension, params};

use super::millis;
use crate::error::{corrupt, sql};
use crate::store::Store;

/// A chat the profile has (had) a session in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct KnownChat {
    /// Platform name.
    pub platform: String,
    /// Chat id.
    pub chat_id: String,
    /// Thread id.
    pub thread_id: Option<String>,
}

impl Store {
    /// Freezes `policy` (JSON) for a routed session; `None` clears it.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn set_session_policy(
        &self,
        session: SessionId,
        policy: Option<&str>,
    ) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "UPDATE gateway_sessions SET policy = ?2 WHERE session_id = ?1",
                params![session.to_string(), policy],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// Freezes whether `session`'s user messages carry their send time
    /// for the model.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn set_session_timestamps(&self, session: SessionId, on: bool) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "UPDATE gateway_sessions SET timestamps = ?2 WHERE session_id = ?1",
                params![session.to_string(), i64::from(on)],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// Whether `session` was created with message timestamps.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn session_timestamps(&self, session: SessionId) -> Result<bool, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT timestamps FROM gateway_sessions WHERE session_id = ?1",
                [session.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(sql)
            .map(|v| v.unwrap_or(0) != 0)
        })
    }

    /// The policy frozen for a routed session (JSON).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn session_policy(&self, session: SessionId) -> Result<Option<String>, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT policy FROM gateway_sessions WHERE session_id = ?1",
                [session.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(Option::flatten)
            .map_err(sql)
        })
    }

    /// Every chat a route points to, optionally of one platform, sorted.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn routed_chats(&self, platform: Option<&str>) -> Result<Vec<KnownChat>, Error> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT DISTINCT platform, chat_id, thread_id FROM gateway_routes \
                     WHERE ?1 IS NULL OR platform = ?1 ORDER BY platform, chat_id, thread_id",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([platform], |row| {
                    Ok(KnownChat {
                        platform: row.get(0)?,
                        chat_id: row.get(1)?,
                        thread_id: row.get(2)?,
                    })
                })
                .map_err(sql)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(sql)
        })
    }

    /// Whether a pinned (kept) session's messages mention `name` (a cached
    /// media file the gateway must not delete).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn pinned_sessions_mention(&self, name: &str) -> Result<bool, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT 1 FROM messages m JOIN sessions s ON s.id = m.session_id \
                 WHERE s.pinned = 1 AND instr(m.content, ?1) > 0 LIMIT 1",
                [name],
                |_| Ok(()),
            )
            .optional()
            .map(|found| found.is_some())
            .map_err(sql)
        })
    }

    /// The session most recently routed for a chat (and thread, when
    /// given), whatever the sender: where a message sent to that chat is
    /// mirrored.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn chat_session(
        &self,
        platform: &str,
        chat_id: &str,
        thread_id: Option<&str>,
    ) -> Result<Option<SessionId>, Error> {
        let found: Option<String> = self.read(|conn| {
            conn.query_row(
                "SELECT session_id FROM gateway_routes WHERE platform = ?1 AND chat_id = ?2 \
                 AND (?3 IS NULL OR thread_id = ?3) ORDER BY last_activity_at DESC LIMIT 1",
                params![platform, chat_id, thread_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)
        })?;
        found
            .map(|id| {
                id.parse()
                    .map_err(|e| corrupt("gateway_routes.session_id", e))
            })
            .transpose()
    }

    /// Records that `sender` on `platform` started a turn of `session` at
    /// `at` (the per-sender spend attribution).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn record_gateway_turn(
        &self,
        session: SessionId,
        platform: &str,
        sender: &str,
        at: Timestamp,
    ) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO gateway_turns (session_id, started_at, platform, sender_id) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![session.to_string(), millis(at), platform, sender],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// USD spent since `since` by the gateway turns `sender` sent on
    /// `platform`. Each metered call belongs to the latest turn started at
    /// or before it in its own session or, for a subagent (`delegate`) or
    /// compression child, in the nearest such ancestor with a gateway turn
    /// (a fork or `/new` successor is a conversation of its own); so in a
    /// thread shared by several people each pays for their own turns.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn sender_cost_since(
        &self,
        platform: &str,
        sender: &str,
        since: Timestamp,
    ) -> Result<f64, Error> {
        self.read(|conn| {
            conn.query_row(
                "WITH RECURSIVE chain(call_id, ts, cost, sid, depth) AS ( \
                   SELECT call_id, ts, cost_usd, session_id, 0 FROM usage_ledger \
                   WHERE ts >= ?1 AND cost_usd IS NOT NULL \
                   UNION ALL \
                   SELECT c.call_id, c.ts, c.cost, s.parent_session_id, c.depth + 1 \
                   FROM chain c JOIN sessions s ON s.id = c.sid \
                   WHERE s.lineage_kind IN ('delegate', 'compression') AND c.depth < 64 \
                 ), owners AS ( \
                   SELECT c.call_id, c.cost, t.platform, t.sender_id, \
                     ROW_NUMBER() OVER (PARTITION BY c.call_id \
                       ORDER BY c.depth ASC, t.started_at DESC) AS pick \
                   FROM chain c JOIN gateway_turns t \
                     ON t.session_id = c.sid AND t.started_at <= c.ts \
                 ) \
                 SELECT COALESCE(SUM(cost), 0.0) FROM owners \
                 WHERE pick = 1 AND platform = ?2 AND sender_id = ?3",
                params![millis(since), platform, sender],
                |row| row.get(0),
            )
            .map_err(sql)
        })
    }
}
