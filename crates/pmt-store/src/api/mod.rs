//! The API server's per-profile state (spec: api-server): [`ApiSession`]
//! rows beside API sessions, the Responses store, idempotent admissions
//! ([`admissions`]) and run records ([`runs`]).

pub mod admissions;
pub mod runs;

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::{OptionalExtension, Row, params};

use crate::error::{corrupt, sql};
use crate::gateway::timestamp;
use crate::sessions::now_ms;
use crate::store::Store;

/// What the API server keeps beside one of its sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiSession {
    /// The session.
    pub session_id: SessionId,
    /// The caller instructions fixed when it was created.
    pub instructions: Option<String>,
    /// The caller's `X-Prometheus-Session-Key`.
    pub session_key: Option<String>,
    /// The Chat Completions conversation hash it continues.
    pub derived_key: Option<String>,
}

/// A stored response of the Responses API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredResponse {
    /// `resp_<ulid>`.
    pub id: String,
    /// The session its turn ran in.
    pub session_id: String,
    /// The named conversation it belongs to.
    pub conversation: Option<String>,
    /// The `response` object (JSON).
    pub body: String,
    /// When it was stored.
    pub created_at: Timestamp,
}

fn api_session(row: &Row<'_>) -> rusqlite::Result<(String, ApiSession)> {
    let id: String = row.get(0)?;
    Ok((
        id,
        ApiSession {
            session_id: SessionId::new(),
            instructions: row.get(1)?,
            session_key: row.get(2)?,
            derived_key: row.get(3)?,
        },
    ))
}

fn with_id((id, mut session): (String, ApiSession)) -> Result<ApiSession, Error> {
    session.session_id = id
        .parse()
        .map_err(|e| corrupt("api_sessions.session_id", e))?;
    Ok(session)
}

fn response(row: &Row<'_>) -> rusqlite::Result<StoredResponse> {
    Ok(StoredResponse {
        id: row.get(0)?,
        session_id: row.get(1)?,
        conversation: row.get(2)?,
        body: row.get(3)?,
        created_at: timestamp(row.get(4)?),
    })
}

const API_SESSION_COLUMNS: &str = "session_id, instructions, session_key, derived_key";
const RESPONSE_COLUMNS: &str = "id, session_id, conversation, body, created_at";

impl Store {
    /// Records (or replaces) what the API server keeps beside a session.
    ///
    /// # Errors
    ///
    /// `store.busy`, `store.sqlite`; `store.constraint` when another
    /// session already has the derived key.
    pub fn put_api_session(&self, session: &ApiSession) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO api_sessions (session_id, instructions, session_key, derived_key, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT (session_id) DO UPDATE SET instructions = excluded.instructions, \
                 session_key = excluded.session_key, derived_key = excluded.derived_key",
                params![
                    session.session_id.to_string(),
                    session.instructions,
                    session.session_key,
                    session.derived_key,
                    now_ms()
                ],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// The API row of session `id`, when it has one.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn api_session(&self, id: SessionId) -> Result<Option<ApiSession>, Error> {
        let found = self.read(|conn| {
            conn.query_row(
                &format!("SELECT {API_SESSION_COLUMNS} FROM api_sessions WHERE session_id = ?1"),
                [id.to_string()],
                api_session,
            )
            .optional()
            .map_err(sql)
        })?;
        found.map(with_id).transpose()
    }

    /// The live session a Chat Completions conversation hash maps to.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn api_session_by_derived(&self, derived: &str) -> Result<Option<SessionId>, Error> {
        self.api_session_where("a.derived_key = ?1", derived)
    }

    /// The newest live session of the caller's session key.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn api_session_by_key(&self, key: &str) -> Result<Option<SessionId>, Error> {
        self.api_session_where("a.session_key = ?1", key)
    }

    fn api_session_where(&self, condition: &str, value: &str) -> Result<Option<SessionId>, Error> {
        let found: Option<String> = self.read(|conn| {
            conn.query_row(
                &format!(
                    "SELECT a.session_id FROM api_sessions a JOIN sessions s ON s.id = a.session_id \
                     WHERE {condition} ORDER BY a.created_at DESC LIMIT 1"
                ),
                [value],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)
        })?;
        found
            .map(|id| {
                id.parse()
                    .map_err(|e| corrupt("api_sessions.session_id", e))
            })
            .transpose()
    }

    /// Stores `response`, then evicts the least recently used responses
    /// beyond `max_stored`.
    ///
    /// # Errors
    ///
    /// `store.busy`, `store.sqlite`.
    pub fn put_api_response(
        &self,
        response: &StoredResponse,
        max_stored: u32,
    ) -> Result<(), Error> {
        self.write(|tx| {
            let now = now_ms();
            tx.execute(
                "INSERT OR REPLACE INTO api_responses (id, session_id, conversation, body, created_at, used_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    response.id,
                    response.session_id,
                    response.conversation,
                    response.body,
                    response.created_at.as_millisecond(),
                    now
                ],
            )
            .map_err(sql)?;
            tx.execute(
                "DELETE FROM api_responses WHERE id NOT IN \
                 (SELECT id FROM api_responses ORDER BY used_at DESC, created_at DESC LIMIT ?1)",
                [i64::from(max_stored)],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// The stored response `id`, marked used.
    ///
    /// # Errors
    ///
    /// `store.busy`, `store.sqlite`.
    pub fn api_response(&self, id: &str) -> Result<Option<StoredResponse>, Error> {
        self.write(|tx| {
            let found = tx
                .query_row(
                    &format!("SELECT {RESPONSE_COLUMNS} FROM api_responses WHERE id = ?1"),
                    [id],
                    response,
                )
                .optional()
                .map_err(sql)?;
            if found.is_some() {
                tx.execute(
                    "UPDATE api_responses SET used_at = ?2 WHERE id = ?1",
                    params![id, now_ms()],
                )
                .map_err(sql)?;
            }
            Ok(found)
        })
    }

    /// The newest response of conversation `name`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn api_conversation_latest(&self, name: &str) -> Result<Option<StoredResponse>, Error> {
        self.read(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {RESPONSE_COLUMNS} FROM api_responses WHERE conversation = ?1 \
                     ORDER BY created_at DESC LIMIT 1"
                ),
                [name],
                response,
            )
            .optional()
            .map_err(sql)
        })
    }

    /// Removes the stored response `id`; whether it existed.
    ///
    /// # Errors
    ///
    /// `store.busy`, `store.sqlite`.
    pub fn delete_api_response(&self, id: &str) -> Result<bool, Error> {
        self.write(|tx| {
            tx.execute("DELETE FROM api_responses WHERE id = ?1", [id])
                .map(|n| n > 0)
                .map_err(sql)
        })
    }
}

#[cfg(test)]
mod tests;
