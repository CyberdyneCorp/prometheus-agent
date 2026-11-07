//! The deterministic turn journal (spec: agent-turn-loop, Deterministic
//! turn journal; Crash resume mid-tool-batch).
//!
//! Each record is its own committed write, made before the side effect it
//! announces, so a session whose newest turn has no `turn_finished` was
//! interrupted by a crash at the point its last record names. Finished
//! tool results are blobs referenced by owner kind [`BLOB_OWNER`] (the
//! session id as owner id) until the session's next turn starts.

use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::params;

use crate::error::sql;
use crate::sessions::now_ms;
use crate::store::Store;

/// The `blob_refs` owner kind of journaled tool results.
pub const BLOB_OWNER: &str = "turn_journal";

/// One journal record kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalKind {
    /// The turn was admitted.
    TurnStarted,
    /// A model request is about to be sent (`data`: canonical hash).
    RequestSent,
    /// Its response arrived.
    ResponseReceived,
    /// A tool call is about to run (`call_id`, `tool`, `effect`,
    /// `idempotent`).
    ToolStarted,
    /// A tool call finished (`call_id`, `data`: the result's blob ref).
    ToolFinished,
    /// The turn ended (`data`: the exit reason).
    TurnFinished,
}

impl JournalKind {
    /// The stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TurnStarted => "turn_started",
            Self::RequestSent => "request_sent",
            Self::ResponseReceived => "response_received",
            Self::ToolStarted => "tool_started",
            Self::ToolFinished => "tool_finished",
            Self::TurnFinished => "turn_finished",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        [
            Self::TurnStarted,
            Self::RequestSent,
            Self::ResponseReceived,
            Self::ToolStarted,
            Self::ToolFinished,
            Self::TurnFinished,
        ]
        .into_iter()
        .find(|k| k.as_str() == name)
    }
}

/// One record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRecord {
    /// The turn (`turn_<ULID>`).
    pub turn_id: String,
    /// What happened.
    pub kind: JournalKind,
    /// The tool call, for tool records.
    pub call_id: Option<String>,
    /// The tool's name (`tool_started`).
    pub tool: Option<String>,
    /// The tool's declared effect (`tool_started`).
    pub effect: Option<String>,
    /// The tool's declared idempotence (`tool_started`).
    pub idempotent: Option<bool>,
    /// The request hash, the result ref or the exit reason.
    pub data: Option<String>,
}

impl JournalRecord {
    /// A record of `kind` with no call or data.
    #[must_use]
    pub fn new(turn_id: &str, kind: JournalKind) -> Self {
        Self {
            turn_id: turn_id.to_owned(),
            kind,
            call_id: None,
            tool: None,
            effect: None,
            idempotent: None,
            data: None,
        }
    }

    /// With `data`.
    #[must_use]
    pub fn with_data(mut self, data: impl Into<String>) -> Self {
        self.data = Some(data.into());
        self
    }

    /// With `call_id`.
    #[must_use]
    pub fn with_call(mut self, call_id: impl Into<String>) -> Self {
        self.call_id = Some(call_id.into());
        self
    }
}

type Raw = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
);

fn parse(raw: Raw) -> Option<JournalRecord> {
    let (turn_id, kind, call_id, tool, effect, idempotent, data) = raw;
    Some(JournalRecord {
        turn_id,
        kind: JournalKind::parse(&kind)?,
        call_id,
        tool,
        effect,
        idempotent: idempotent.map(|v| v != 0),
        data,
    })
}

impl Store {
    /// Commits one record of `session` (a `turn_started` first drops the
    /// records of the session's earlier turns).
    ///
    /// # Errors
    ///
    /// `store.sqlite` (the caller must not start the side effect then).
    pub fn journal(&self, session: SessionId, record: &JournalRecord) -> Result<(), Error> {
        let session = session.to_string();
        self.write(|tx| {
            if record.kind == JournalKind::TurnStarted {
                for statement in [
                    "DELETE FROM turn_journal WHERE session_id = ?1",
                    "DELETE FROM blob_refs WHERE owner_kind = 'turn_journal' AND owner_id = ?1",
                ] {
                    tx.execute(statement, [&session]).map_err(sql)?;
                }
            }
            tx.execute(
                "INSERT INTO turn_journal (session_id, turn_id, kind, call_id, tool, effect, \
                 idempotent, data, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    session,
                    record.turn_id,
                    record.kind.as_str(),
                    record.call_id,
                    record.tool,
                    record.effect,
                    record.idempotent.map(i64::from),
                    record.data,
                    now_ms(),
                ],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// The records of `session`'s newest turn, in order.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn journal_of(&self, session: SessionId) -> Result<Vec<JournalRecord>, Error> {
        let session = session.to_string();
        self.read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT turn_id, kind, call_id, tool, effect, idempotent, data \
                     FROM turn_journal WHERE session_id = ?1 AND turn_id = \
                     (SELECT turn_id FROM turn_journal WHERE session_id = ?1 \
                      ORDER BY id DESC LIMIT 1) ORDER BY id",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([&session], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                })
                .map_err(sql)?;
            let mut out = Vec::new();
            for raw in rows {
                out.extend(parse(raw.map_err(sql)?));
            }
            Ok(out)
        })
    }

    /// The newest turn of `session` when it never finished (a crash).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn unfinished_turn(&self, session: SessionId) -> Result<Option<Vec<JournalRecord>>, Error> {
        let records = self.journal_of(session)?;
        let finished = records.iter().any(|r| r.kind == JournalKind::TurnFinished);
        Ok((!records.is_empty() && !finished).then_some(records))
    }
}
