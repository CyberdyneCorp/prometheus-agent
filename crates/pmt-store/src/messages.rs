//! The canonical transcript (spec: session-storage, Transactional message
//! persistence; Single session identity format).
//!
//! A batch (one turn's rows) is inserted together with the session's
//! counters in one write transaction, so a crash or a failing row leaves
//! all of the batch or none of it. Payloads over [`SPILL_BYTES`] are moved
//! to the blob store before the transaction and referenced by SHA-256.

use std::path::Path;

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_core::id::{MessageId, SessionId};
use rusqlite::{Connection, Row, Transaction, params};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::blobs::{self, BlobRef, SPILL_BYTES};
use crate::contention::WriteClass;
use crate::error::{corrupt, sql};
use crate::index;
use crate::model::Role;
use crate::sessions::{self, now_ms};
use crate::store::Store;

/// A message to append.
#[derive(Debug, Clone, PartialEq)]
pub struct NewMessage {
    /// Durable id; keep the original when importing or copying.
    pub id: MessageId,
    /// Author role.
    pub role: Role,
    /// Text content.
    pub content: Option<String>,
    /// Tool calls requested by an assistant message.
    pub tool_calls: Option<Value>,
    /// Reasoning items.
    pub reasoning: Option<Value>,
    /// Provider-specific opaque payload, replayed verbatim.
    pub provider_payload: Option<Value>,
    /// The call a tool result answers.
    pub tool_call_id: Option<String>,
    /// The tool that produced a result.
    pub tool_name: Option<String>,
    /// Host metadata (for example the `compaction_summary` flag), never
    /// sent on the wire.
    pub metadata: Option<Value>,
}

impl NewMessage {
    /// A message with a fresh id and text content.
    #[must_use]
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            id: MessageId::new(),
            role,
            content: Some(content.into()),
            tool_calls: None,
            reasoning: None,
            provider_payload: None,
            tool_call_id: None,
            tool_name: None,
            metadata: None,
        }
    }

    fn tool_call_count(&self) -> i64 {
        match &self.tool_calls {
            None | Some(Value::Null) => 0,
            Some(Value::Array(calls)) => i64::try_from(calls.len()).unwrap_or(i64::MAX),
            Some(_) => 1,
        }
    }

    /// SHA-256 over the canonical JSON of everything the model sees; the
    /// compare-and-swap value of compaction.
    fn content_hash(&self) -> String {
        let canonical = serde_json::json!({
            "role": self.role.as_str(),
            "content": self.content,
            "tool_calls": self.tool_calls,
            "reasoning": self.reasoning,
            "provider_payload": self.provider_payload,
            "tool_call_id": self.tool_call_id,
            "tool_name": self.tool_name,
        });
        blobs::hex(&Sha256::digest(canonical.to_string().as_bytes()))
    }
}

/// A stored message.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// Durable id.
    pub id: MessageId,
    /// Owning session.
    pub session_id: SessionId,
    /// Position in the session, strictly increasing.
    pub seq: u64,
    /// Author role.
    pub role: Role,
    /// Text content (loaded from the blob store when spilled).
    pub content: Option<String>,
    /// Tool calls.
    pub tool_calls: Option<Value>,
    /// Reasoning items.
    pub reasoning: Option<Value>,
    /// Provider-specific payload.
    pub provider_payload: Option<Value>,
    /// The call a tool result answers.
    pub tool_call_id: Option<String>,
    /// The tool that produced a result.
    pub tool_name: Option<String>,
    /// Host metadata.
    pub metadata: Option<Value>,
    /// SHA-256 of the model-visible fields.
    pub content_hash: String,
    /// Insert time.
    pub created_at: Timestamp,
    /// Part of the active transcript.
    pub active: bool,
    /// Replaced by a compaction summary (still readable).
    pub compacted: bool,
    /// The compaction epoch that replaced it.
    pub compaction_epoch: Option<u32>,
    /// The blob holding `content` when it was spilled.
    pub content_ref: Option<BlobRef>,
}

/// Which rows a transcript read returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// The active transcript (what the model sees).
    Active,
    /// Every row, including rewound and compacted ones.
    All,
}

/// A payload column: inline text, or a blob reference.
enum Stored {
    Inline(String),
    Blob(BlobRef),
}

impl Stored {
    fn columns(stored: Option<&Self>) -> (Option<&str>, Option<String>) {
        match stored {
            None => (None, None),
            Some(Self::Inline(text)) => (Some(text), None),
            Some(Self::Blob(blob)) => (None, Some(blob.to_string())),
        }
    }
}

/// A message with its payloads placed (inline or spilled).
pub(crate) struct Prepared {
    message: NewMessage,
    content: Option<Stored>,
    tool_calls: Option<Stored>,
    reasoning: Option<Stored>,
    provider_payload: Option<Stored>,
    hash: String,
}

fn place(blob_dir: &Path, text: Option<String>) -> Result<Option<Stored>, Error> {
    text.map(|text| {
        if text.len() > SPILL_BYTES {
            blobs::put(blob_dir, text.as_bytes()).map(Stored::Blob)
        } else {
            Ok(Stored::Inline(text))
        }
    })
    .transpose()
}

/// Spills large payloads; runs before the write transaction.
pub(crate) fn prepare(blob_dir: &Path, message: &NewMessage) -> Result<Prepared, Error> {
    let json = |value: &Option<Value>| value.as_ref().map(Value::to_string);
    Ok(Prepared {
        content: place(blob_dir, message.content.clone())?,
        tool_calls: place(blob_dir, json(&message.tool_calls))?,
        reasoning: place(blob_dir, json(&message.reasoning))?,
        provider_payload: place(blob_dir, json(&message.provider_payload))?,
        hash: message.content_hash(),
        message: message.clone(),
    })
}

/// The next free `seq` of a session.
pub(crate) fn next_seq(tx: &Connection, session: SessionId) -> Result<i64, Error> {
    tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE session_id = ?1",
        [session.to_string()],
        |row| row.get(0),
    )
    .map_err(sql)
}

/// Inserts `rows` at `first_seq..` and bumps the session's counters.
pub(crate) fn insert(
    tx: &Transaction<'_>,
    session: SessionId,
    rows: &[Prepared],
    first_seq: i64,
) -> Result<Vec<u64>, Error> {
    let now = now_ms();
    let mut stmt = tx
        .prepare_cached(
            "INSERT INTO messages (id, session_id, seq, role, content, content_ref, tool_calls, \
             tool_calls_ref, reasoning, reasoning_ref, provider_payload, provider_payload_ref, \
             tool_call_id, tool_name, metadata, content_hash, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        )
        .map_err(sql)?;
    let mut seqs = Vec::with_capacity(rows.len());
    let mut indexed = Vec::with_capacity(rows.len());
    let mut tool_calls = 0;
    for (seq, row) in (first_seq..).zip(rows) {
        let m = &row.message;
        let (content, content_ref) = Stored::columns(row.content.as_ref());
        let (calls, calls_ref) = Stored::columns(row.tool_calls.as_ref());
        let (reasoning, reasoning_ref) = Stored::columns(row.reasoning.as_ref());
        let (payload, payload_ref) = Stored::columns(row.provider_payload.as_ref());
        stmt.execute(params![
            m.id.to_string(),
            session.to_string(),
            seq,
            m.role.as_str(),
            content,
            content_ref,
            calls,
            calls_ref,
            reasoning,
            reasoning_ref,
            payload,
            payload_ref,
            m.tool_call_id,
            m.tool_name,
            m.metadata.as_ref().map(Value::to_string),
            row.hash,
            now,
        ])
        .map_err(sql)?;
        if let Some(text) = m.content.as_deref() {
            indexed.push((tx.last_insert_rowid(), text));
        }
        tool_calls += m.tool_call_count();
        seqs.push(u64::try_from(seq).unwrap_or(0));
    }
    let changed = tx
        .execute(
            "UPDATE sessions SET message_count = message_count + ?2, \
             tool_call_count = tool_call_count + ?3, last_activity_at = ?4 WHERE id = ?1",
            params![
                session.to_string(),
                i64::try_from(rows.len()).unwrap_or(i64::MAX),
                tool_calls,
                now
            ],
        )
        .map_err(sql)?;
    if changed == 0 {
        return Err(sessions::not_found(session));
    }
    index::add(tx, &indexed)?;
    Ok(seqs)
}

const COLUMNS: &str = "id, session_id, seq, role, content, content_ref, tool_calls, tool_calls_ref, \
    reasoning, reasoning_ref, provider_payload, provider_payload_ref, tool_call_id, tool_name, \
    metadata, content_hash, created_at, active, compacted, compaction_epoch";

fn message_from_row(blob_dir: &Path, row: &Row<'_>) -> Result<Message, Error> {
    let text = |i: usize| row.get::<_, Option<String>>(i).map_err(sql);
    let payload = |i: usize| -> Result<Option<String>, Error> {
        if let Some(inline) = text(i)? {
            return Ok(Some(inline));
        }
        let Some(reference) = text(i + 1)? else {
            return Ok(None);
        };
        let blob =
            BlobRef::parse(&reference).ok_or_else(|| corrupt("blob reference", &reference))?;
        let bytes = blobs::get(blob_dir, &blob)?;
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    };
    let json = |i: usize, column: &str| -> Result<Option<Value>, Error> {
        payload(i)?
            .map(|t| serde_json::from_str(&t).map_err(|e| corrupt(column, e)))
            .transpose()
    };
    let int = |i: usize| row.get::<_, i64>(i).map_err(sql);
    Ok(Message {
        id: MessageId::parse(&row.get::<_, String>(0).map_err(sql)?)?,
        session_id: SessionId::parse(&row.get::<_, String>(1).map_err(sql)?)?,
        seq: u64::try_from(int(2)?).unwrap_or(0),
        role: row.get::<_, String>(3).map_err(sql)?.parse()?,
        content: payload(4)?,
        content_ref: text(5)?.as_deref().and_then(BlobRef::parse),
        tool_calls: json(6, "tool_calls")?,
        reasoning: json(8, "reasoning")?,
        provider_payload: json(10, "provider_payload")?,
        tool_call_id: text(12)?,
        tool_name: text(13)?,
        metadata: text(14)?
            .map(|t| serde_json::from_str(&t).map_err(|e| corrupt("metadata", e)))
            .transpose()?,
        content_hash: row.get(15).map_err(sql)?,
        created_at: Timestamp::from_millisecond(int(16)?).unwrap_or(Timestamp::UNIX_EPOCH),
        active: row.get(17).map_err(sql)?,
        compacted: row.get(18).map_err(sql)?,
        compaction_epoch: row
            .get::<_, Option<i64>>(19)
            .map_err(sql)?
            .and_then(|e| u32::try_from(e).ok()),
    })
}

pub(crate) fn load(
    conn: &Connection,
    blob_dir: &Path,
    session: SessionId,
    visibility: Visibility,
) -> Result<Vec<Message>, Error> {
    let mut messages = Vec::new();
    for_each(conn, blob_dir, session, visibility, |m| {
        messages.push(m);
        Ok(())
    })?;
    Ok(messages)
}

/// Calls `each` with every row of the transcript in `seq` order.
pub(crate) fn for_each(
    conn: &Connection,
    blob_dir: &Path,
    session: SessionId,
    visibility: Visibility,
    mut each: impl FnMut(Message) -> Result<(), Error>,
) -> Result<(), Error> {
    let filter = match visibility {
        Visibility::Active => " AND active = 1",
        Visibility::All => "",
    };
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {COLUMNS} FROM messages WHERE session_id = ?1{filter} ORDER BY seq"
        ))
        .map_err(sql)?;
    let mut rows = stmt.query([session.to_string()]).map_err(sql)?;
    while let Some(row) = rows.next().map_err(sql)? {
        each(message_from_row(blob_dir, row)?)?;
    }
    Ok(())
}

impl Store {
    /// Appends one message (and its counters) in one transaction; returns
    /// its `seq`.
    ///
    /// # Errors
    ///
    /// As [`Store::append_messages`].
    pub fn append_message(&self, session: SessionId, message: &NewMessage) -> Result<u64, Error> {
        self.append_messages(session, std::slice::from_ref(message))
            .map(|seqs| seqs[0])
    }

    /// Appends a batch (a turn's rows) all-or-none, with consecutive
    /// `seq`s after the session's last row; returns the `seq`s.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.constraint` (for example a duplicate
    /// message id; nothing from the batch is stored), `store.io`,
    /// `store.schema_too_new`, `store.sqlite`.
    pub fn append_messages(
        &self,
        session: SessionId,
        messages: &[NewMessage],
    ) -> Result<Vec<u64>, Error> {
        self.ensure_writable()?;
        let blob_dir = self.location().blobs();
        let rows = messages
            .iter()
            .map(|m| prepare(blob_dir, m))
            .collect::<Result<Vec<_>, _>>()?;
        self.write_as(WriteClass::Transcript, |tx| {
            if sessions::load(tx, session)?.is_none() {
                return Err(sessions::not_found(session));
            }
            let first = next_seq(tx, session)?;
            insert(tx, session, &rows, first)
        })
    }

    /// A session's transcript in `seq` order.
    ///
    /// # Errors
    ///
    /// `store.blob_missing`, `store.blob_corrupt`, `store.sqlite`.
    pub fn messages(
        &self,
        session: SessionId,
        visibility: Visibility,
    ) -> Result<Vec<Message>, Error> {
        let blob_dir = self.location().blobs().to_path_buf();
        self.read(|conn| load(conn, &blob_dir, session, visibility))
    }
}
