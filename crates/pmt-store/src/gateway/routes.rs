//! Conversation routes and the gateway state of routed sessions (spec:
//! messaging-gateway, Conversations never rotate on time; Crash recovery
//! and clean shutdown marker; Stuck-loop suspension). A route is replaced
//! only by an explicit transition that records why; pruning a route never
//! touches its session.

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::{OptionalExtension, Row, params};

use super::{millis, timestamp};
use crate::error::{corrupt, sql};
use crate::store::Store;

/// Why a key's session was replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetReason {
    /// `/new`.
    New,
    /// `/reset`.
    Reset,
    /// The routed session was suspended (stuck loop).
    Suspended,
    /// The route was pruned, or its session ended.
    StaleRoute,
    /// A changed per-chat policy applied at once.
    Policy,
}

impl ResetReason {
    /// The stored value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Reset => "reset",
            Self::Suspended => "suspended",
            Self::StaleRoute => "stale_route",
            Self::Policy => "policy",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "new" => Self::New,
            "reset" => Self::Reset,
            "suspended" => Self::Suspended,
            "stale_route" => Self::StaleRoute,
            "policy" => Self::Policy,
            _ => return None,
        })
    }
}

/// A route to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRoute {
    /// The conversation key.
    pub key: String,
    /// The session it routes to.
    pub session_id: SessionId,
    /// Platform name.
    pub platform: String,
    /// Chat id.
    pub chat_id: String,
    /// Thread id.
    pub thread_id: Option<String>,
}

/// A stored route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// The conversation key.
    pub key: String,
    /// The session it routes to.
    pub session_id: SessionId,
    /// Platform name.
    pub platform: String,
    /// Chat id.
    pub chat_id: String,
    /// Thread id.
    pub thread_id: Option<String>,
    /// When the route was created.
    pub created_at: Timestamp,
    /// Last human activity on it.
    pub last_activity_at: Timestamp,
}

/// The gateway state of a routed session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedSession {
    /// The session.
    pub session_id: SessionId,
    /// The key it was created for.
    pub route_key: String,
    /// Why it replaced the key's previous session.
    pub reset_reason: Option<ResetReason>,
    /// Suspended after too many restarts during its turns.
    pub suspended: bool,
    /// A turn of it is running (cleared when the turn ends).
    pub turn_active: bool,
    /// Consecutive daemon restarts during its turns.
    pub restart_count: u32,
    /// Its last turn was interrupted by a restart (`/retry` runs it again).
    pub resume_pending: bool,
}

/// A session whose turn was running when the daemon last stopped without
/// a clean shutdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interrupted {
    /// The session.
    pub session_id: SessionId,
    /// Its conversation key.
    pub route_key: String,
    /// The route's chat, when the key still routes to it.
    pub route: Option<Route>,
    /// It reached the stuck-loop threshold and is now suspended.
    pub suspended: bool,
}

const ROUTE_COLUMNS: &str =
    "key, session_id, platform, chat_id, thread_id, created_at, last_activity_at";

fn route_from_row(row: &Row<'_>) -> Result<Route, Error> {
    let session: String = row.get(1).map_err(sql)?;
    Ok(Route {
        key: row.get(0).map_err(sql)?,
        session_id: session
            .parse()
            .map_err(|e| corrupt("gateway_routes.session_id", e))?,
        platform: row.get(2).map_err(sql)?,
        chat_id: row.get(3).map_err(sql)?,
        thread_id: row.get(4).map_err(sql)?,
        created_at: timestamp(row.get(5).map_err(sql)?),
        last_activity_at: timestamp(row.get(6).map_err(sql)?),
    })
}

fn route_of(conn: &rusqlite::Connection, key: &str) -> Result<Option<Route>, Error> {
    conn.query_row(
        &format!("SELECT {ROUTE_COLUMNS} FROM gateway_routes WHERE key = ?1"),
        [key],
        |row| Ok(route_from_row(row)),
    )
    .optional()
    .map_err(sql)?
    .transpose()
}

impl Store {
    /// The route of `key`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn route(&self, key: &str) -> Result<Option<Route>, Error> {
        self.read(|conn| route_of(conn, key))
    }

    /// Routes `route.key` to `route.session_id` (replacing any previous
    /// route) and records the session's gateway state with `reset` (why it
    /// replaced the previous session, `None` for a key's first session).
    ///
    /// # Errors
    ///
    /// `store.sqlite` (also when the session does not exist).
    pub fn bind_route(&self, route: &NewRoute, reset: Option<ResetReason>) -> Result<(), Error> {
        let now = millis(Timestamp::now());
        self.write(|tx| {
            tx.execute(
                "INSERT INTO gateway_routes (key, session_id, platform, chat_id, thread_id, \
                 created_at, last_activity_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6) \
                 ON CONFLICT (key) DO UPDATE SET session_id = excluded.session_id, \
                 created_at = excluded.created_at, last_activity_at = excluded.last_activity_at",
                params![
                    route.key,
                    route.session_id.to_string(),
                    route.platform,
                    route.chat_id,
                    route.thread_id,
                    now
                ],
            )
            .map_err(sql)?;
            tx.execute(
                "INSERT INTO gateway_sessions (session_id, route_key, reset_reason) \
                 VALUES (?1, ?2, ?3) ON CONFLICT (session_id) DO NOTHING",
                params![
                    route.session_id.to_string(),
                    route.key,
                    reset.map(ResetReason::as_str)
                ],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// Records human activity on `key` (internal events never call it).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn touch_route(&self, key: &str, at: Timestamp) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "UPDATE gateway_routes SET last_activity_at = ?2 WHERE key = ?1",
                params![key, millis(at)],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// Removes the routes last active before `before`; their sessions and
    /// transcripts stay. Returns how many were removed.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn prune_routes(&self, before: Timestamp) -> Result<usize, Error> {
        self.write(|tx| {
            tx.execute(
                "DELETE FROM gateway_routes WHERE last_activity_at < ?1",
                [millis(before)],
            )
            .map_err(sql)
        })
    }

    /// The gateway state of `session`, when the gateway routed to it.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn routed_session(&self, session: SessionId) -> Result<Option<RoutedSession>, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT session_id, route_key, reset_reason, state, turn_active, restart_count, \
                 resume_pending FROM gateway_sessions WHERE session_id = ?1",
                [session.to_string()],
                |row| Ok(routed_from_row(row)),
            )
            .optional()
            .map_err(sql)?
            .transpose()
        })
    }

    /// The session most recently created for `key` (the route may have
    /// been pruned since): the parent of the key's next session.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn latest_routed_session(&self, key: &str) -> Result<Option<SessionId>, Error> {
        let found: Option<String> = self.read(|conn| {
            conn.query_row(
                "SELECT session_id FROM gateway_sessions WHERE route_key = ?1 \
                 ORDER BY rowid DESC LIMIT 1",
                [key],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)
        })?;
        found
            .map(|id| {
                id.parse()
                    .map_err(|e| corrupt("gateway_sessions.session_id", e))
            })
            .transpose()
    }

    /// Marks a turn of `session` as running (`true`) or finished. A
    /// finished turn resets the restart counter and the pending resume.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn set_turn_active(&self, session: SessionId, active: bool) -> Result<(), Error> {
        let sql_text = if active {
            "UPDATE gateway_sessions SET turn_active = 1 WHERE session_id = ?1"
        } else {
            "UPDATE gateway_sessions SET turn_active = 0, restart_count = 0, resume_pending = 0, \
             resume_reason = NULL WHERE session_id = ?1"
        };
        self.write(|tx| {
            tx.execute(sql_text, [session.to_string()]).map_err(sql)?;
            Ok(())
        })
    }

    /// Crash recovery: every session whose turn was running when the
    /// daemon stopped uncleanly is marked `resume_pending` (reason
    /// `restart_interrupted`) and its restart counter grows; a counter
    /// reaching `threshold` suspends the session.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn recover_interrupted(&self, threshold: u32) -> Result<Vec<Interrupted>, Error> {
        self.write(|tx| {
            let mut stmt = tx
                .prepare(
                    "SELECT session_id, route_key FROM gateway_sessions WHERE turn_active = 1 \
                     ORDER BY session_id",
                )
                .map_err(sql)?;
            let rows: Vec<(String, String)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(sql)?
                .collect::<Result<_, _>>()
                .map_err(sql)?;
            drop(stmt);
            tx.execute(
                "UPDATE gateway_sessions SET turn_active = 0, restart_count = restart_count + 1, \
                 resume_pending = 1, resume_reason = 'restart_interrupted', \
                 state = CASE WHEN restart_count + 1 >= ?1 THEN 'suspended' ELSE state END \
                 WHERE turn_active = 1",
                [threshold],
            )
            .map_err(sql)?;
            let mut found = Vec::new();
            for (session, key) in rows {
                let session_id: SessionId = session
                    .parse()
                    .map_err(|e| corrupt("gateway_sessions.session_id", e))?;
                let suspended: String = tx
                    .query_row(
                        "SELECT state FROM gateway_sessions WHERE session_id = ?1",
                        [&session],
                        |row| row.get(0),
                    )
                    .map_err(sql)?;
                let route = route_of(tx, &key)?.filter(|r| r.session_id == session_id);
                found.push(Interrupted {
                    session_id,
                    route_key: key,
                    route,
                    suspended: suspended == "suspended",
                });
            }
            Ok(found)
        })
    }
}

fn routed_from_row(row: &Row<'_>) -> Result<RoutedSession, Error> {
    let session: String = row.get(0).map_err(sql)?;
    let reason: Option<String> = row.get(2).map_err(sql)?;
    let state: String = row.get(3).map_err(sql)?;
    Ok(RoutedSession {
        session_id: session
            .parse()
            .map_err(|e| corrupt("gateway_sessions.session_id", e))?,
        route_key: row.get(1).map_err(sql)?,
        reset_reason: reason
            .map(|r| {
                ResetReason::parse(&r).ok_or_else(|| corrupt("gateway_sessions.reset_reason", r))
            })
            .transpose()?,
        suspended: state == "suspended",
        turn_active: row.get::<_, i64>(4).map_err(sql)? != 0,
        restart_count: u32::try_from(row.get::<_, i64>(5).map_err(sql)?)
            .map_err(|e| corrupt("gateway_sessions.restart_count", e))?,
        resume_pending: row.get::<_, i64>(6).map_err(sql)? != 0,
    })
}
