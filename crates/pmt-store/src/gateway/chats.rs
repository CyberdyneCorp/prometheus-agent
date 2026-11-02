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

    /// USD spent since `since` by the turns of gateway sessions that
    /// `sender` started on `platform` (their whole lineage, subagents
    /// included, through the ledger's root session).
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
                "SELECT COALESCE(SUM(u.cost_usd), 0.0) FROM usage_ledger u \
                 JOIN sessions s ON s.id = u.root_session_id \
                 WHERE u.ts >= ?1 AND json_extract(s.origin, '$.kind') = 'gateway' \
                 AND json_extract(s.origin, '$.platform') = ?2 \
                 AND json_extract(s.origin, '$.user') = ?3",
                params![millis(since), platform, sender],
                |row| row.get(0),
            )
            .map_err(sql)
        })
    }
}
