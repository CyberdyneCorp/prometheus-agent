//! Agent interop state (spec: agent-interop): inbound A2A conversation
//! bindings, outbound `a2a_*` exchanges and peer gateway probe results.
//!
//! A binding is the only way an inbound A2A task finds its session again:
//! the key holds the peer identity, tenant and context id, so two peers
//! reusing one context id never share a transcript, and nothing is ever
//! matched by session title.

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::{OptionalExtension as _, params};

use crate::error::{corrupt, sql};
use crate::store::Store;

/// One inbound A2A conversation binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2aBinding {
    /// `<profile>:a2a:<peer>[@<tenant>]:<context>`.
    pub key: String,
    /// The session its turns run in.
    pub session_id: SessionId,
    /// The authenticated peer identity.
    pub peer: String,
    /// The request's tenant, when it named one.
    pub tenant: Option<String>,
    /// The peer's `contextId`.
    pub context_id: String,
}

/// One outbound A2A exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2aCall {
    /// The conversation.
    pub context_id: String,
    /// The configured agent name, or the URL called.
    pub agent: String,
    /// The agent's URL.
    pub url: String,
    /// The remote task id, when the agent answered with a task.
    pub task_id: Option<String>,
    /// What was sent.
    pub message: String,
    /// The reply, on success.
    pub reply: Option<String>,
    /// Why it failed.
    pub error: Option<String>,
    /// How long the call took.
    pub duration_ms: u64,
    /// When it was made.
    pub at: Timestamp,
}

/// One agent's call metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2aMetrics {
    /// The agent.
    pub agent: String,
    /// Calls made.
    pub calls: u64,
    /// Calls that failed.
    pub failures: u64,
    /// Mean duration.
    pub mean_ms: u64,
}

/// One saved outbound conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2aConversation {
    /// The conversation.
    pub context_id: String,
    /// The agent it is with.
    pub agent: String,
    /// Exchanges in it.
    pub exchanges: u64,
    /// The last exchange.
    pub last_at: Timestamp,
}

/// The last probe of a peer gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerProbe {
    /// The peer.
    pub name: String,
    /// When it was probed.
    pub at: Timestamp,
    /// Whether it answered with its capabilities.
    pub ok: bool,
    /// What it answered (`ok`, `unreachable: ...`, `HTTP 500`).
    pub detail: String,
}

fn time(column: &str, ms: i64) -> Result<Timestamp, Error> {
    Timestamp::from_millisecond(ms).map_err(|e| corrupt(column, e))
}

impl Store {
    /// The session bound to A2A conversation `key`, if any.
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.corrupt`.
    pub fn a2a_session(&self, key: &str) -> Result<Option<SessionId>, Error> {
        let id: Option<String> = self.read(|conn| {
            conn.query_row(
                "SELECT session_id FROM a2a_bindings WHERE key = ?1",
                [key],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql)
        })?;
        id.map(|id| SessionId::parse(&id).map_err(|e| corrupt("a2a_bindings.session_id", e)))
            .transpose()
    }

    /// Binds `binding.key` to its session (replacing a binding whose
    /// session was deleted).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn bind_a2a(&self, binding: &A2aBinding) -> Result<(), Error> {
        let now = Timestamp::now().as_millisecond();
        self.write(|tx| {
            tx.execute(
                "INSERT INTO a2a_bindings (key, session_id, peer, tenant, context_id, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT (key) DO UPDATE SET session_id = excluded.session_id",
                params![
                    binding.key,
                    binding.session_id.to_string(),
                    binding.peer,
                    binding.tenant,
                    binding.context_id,
                    now
                ],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// Every inbound A2A binding, oldest first.
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.corrupt`.
    pub fn a2a_bindings(&self) -> Result<Vec<A2aBinding>, Error> {
        type Row = (String, String, String, Option<String>, String);
        let rows: Vec<Row> = self.read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT key, session_id, peer, tenant, context_id FROM a2a_bindings \
                     ORDER BY created_at, key",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .map_err(sql)?;
            rows.collect::<Result<_, _>>().map_err(sql)
        })?;
        rows.into_iter()
            .map(|(key, session, peer, tenant, context_id)| {
                Ok(A2aBinding {
                    key,
                    session_id: SessionId::parse(&session)
                        .map_err(|e| corrupt("a2a_bindings.session_id", e))?,
                    peer,
                    tenant,
                    context_id,
                })
            })
            .collect()
    }

    /// Imported Hermes A2A sessions (tag `a2a-import`), which no binding
    /// can ever attach.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn unbound_a2a_imports(&self) -> Result<u64, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM sessions WHERE source_tag = ?1",
                [crate::sessions::A2A_IMPORT_TAG],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| u64::try_from(n).unwrap_or(0))
            .map_err(sql)
        })
    }

    /// Records one outbound exchange.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn record_a2a_call(&self, call: &A2aCall) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO a2a_calls (context_id, agent, url, task_id, message, reply, error, \
                 duration_ms, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    call.context_id,
                    call.agent,
                    call.url,
                    call.task_id,
                    call.message,
                    call.reply,
                    call.error,
                    i64::try_from(call.duration_ms).unwrap_or(i64::MAX),
                    call.at.as_millisecond()
                ],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// The exchanges of conversation `context_id`, oldest first.
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.corrupt`.
    pub fn a2a_history(&self, context_id: &str) -> Result<Vec<A2aCall>, Error> {
        type Row = (
            String,
            String,
            Option<String>,
            String,
            Option<String>,
            Option<String>,
            i64,
            i64,
        );
        let rows: Vec<Row> = self.read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT agent, url, task_id, message, reply, error, duration_ms, at \
                     FROM a2a_calls WHERE context_id = ?1 ORDER BY id",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([context_id], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                })
                .map_err(sql)?;
            rows.collect::<Result<_, _>>().map_err(sql)
        })?;
        rows.into_iter()
            .map(|(agent, url, task_id, message, reply, error, ms, at)| {
                Ok(A2aCall {
                    context_id: context_id.to_owned(),
                    agent,
                    url,
                    task_id,
                    message,
                    reply,
                    error,
                    duration_ms: u64::try_from(ms).unwrap_or(0),
                    at: time("a2a_calls.at", at)?,
                })
            })
            .collect()
    }

    /// Saved outbound conversations, most recent first (at most `limit`).
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.corrupt`.
    pub fn a2a_conversations(&self, limit: u32) -> Result<Vec<A2aConversation>, Error> {
        let rows: Vec<(String, String, i64, i64)> = self.read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT context_id, MAX(agent), COUNT(*), MAX(at) FROM a2a_calls \
                     GROUP BY context_id ORDER BY MAX(at) DESC LIMIT ?1",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([limit], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })
                .map_err(sql)?;
            rows.collect::<Result<_, _>>().map_err(sql)
        })?;
        rows.into_iter()
            .map(|(context_id, agent, n, at)| {
                Ok(A2aConversation {
                    context_id,
                    agent,
                    exchanges: u64::try_from(n).unwrap_or(0),
                    last_at: time("a2a_calls.at", at)?,
                })
            })
            .collect()
    }

    /// Call metrics per agent, by name.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn a2a_metrics(&self) -> Result<Vec<A2aMetrics>, Error> {
        let rows: Vec<(String, i64, i64, f64)> = self.read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT agent, COUNT(*), SUM(error IS NOT NULL), AVG(duration_ms) \
                     FROM a2a_calls GROUP BY agent ORDER BY agent",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .map_err(sql)?;
            rows.collect::<Result<_, _>>().map_err(sql)
        })?;
        Ok(rows
            .into_iter()
            .map(|(agent, calls, failures, mean)| A2aMetrics {
                agent,
                calls: u64::try_from(calls).unwrap_or(0),
                failures: u64::try_from(failures).unwrap_or(0),
                // Durations are milliseconds of one call: far below 2^52.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                mean_ms: mean.max(0.0).round() as u64,
            })
            .collect())
    }

    /// Records the probe of peer `name`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn record_peer_probe(&self, name: &str, ok: bool, detail: &str) -> Result<(), Error> {
        let now = Timestamp::now().as_millisecond();
        self.write(|tx| {
            tx.execute(
                "INSERT INTO peer_probes (name, at, ok, detail) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (name) DO UPDATE SET at = excluded.at, ok = excluded.ok, \
                 detail = excluded.detail",
                params![name, now, ok, detail],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// The last probe of peer `name`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`, `store.corrupt`.
    pub fn peer_probe(&self, name: &str) -> Result<Option<PeerProbe>, Error> {
        let row: Option<(i64, bool, String)> = self.read(|conn| {
            conn.query_row(
                "SELECT at, ok, detail FROM peer_probes WHERE name = ?1",
                [name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(sql)
        })?;
        row.map(|(at, ok, detail)| {
            Ok(PeerProbe {
                name: name.to_owned(),
                at: time("peer_probes.at", at)?,
                ok,
                detail,
            })
        })
        .transpose()
    }

    /// Forgets the probe of peer `name`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn forget_peer_probe(&self, name: &str) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute("DELETE FROM peer_probes WHERE name = ?1", [name])
                .map(drop)
                .map_err(sql)
        })
    }
}
