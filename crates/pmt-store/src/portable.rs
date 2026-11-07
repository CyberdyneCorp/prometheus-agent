//! Writing imported sessions (spec: session-storage, Portable export and
//! import). The importer (prometheus `sessions import`) parses and checks
//! the whole file first; [`Store::import_portable`] then writes every
//! session, message, usage row and blob reference in one transaction, so
//! an import is all-or-none.

use std::collections::BTreeSet;

use pmt_core::error::Error;
use pmt_core::id::{MessageId, SessionId};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::blobs::BlobRef;
use crate::error::sql;
use crate::messages::{self, Message, NewMessage};
use crate::model::{EndReason, LineageKind, TitleSource};
use crate::sessions::{self, Session};
use crate::store::Store;
use crate::usage::UsageEvent;

/// One session to import, with its rows (ids already final).
#[derive(Debug, Clone)]
pub struct Portable {
    /// The session record.
    pub session: Session,
    /// Its messages in order.
    pub messages: Vec<Message>,
    /// Its usage ledger rows.
    pub usage: Vec<UsageEvent>,
    /// Blobs its messages reference beyond their own payloads
    /// (attachments).
    pub blob_refs: Vec<(MessageId, BlobRef)>,
}

/// Ids an import would collide with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Collisions {
    /// Session ids that exist.
    pub sessions: Vec<String>,
    /// Message ids that exist.
    pub messages: Vec<String>,
    /// Usage call ids that exist.
    pub calls: Vec<String>,
}

impl Collisions {
    /// No collision at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty() && self.messages.is_empty() && self.calls.is_empty()
    }
}

fn existing(
    conn: &Connection,
    table: &str,
    column: &str,
    ids: &[String],
) -> Result<Vec<String>, Error> {
    let mut stmt = conn
        .prepare_cached(&format!("SELECT 1 FROM {table} WHERE {column} = ?1"))
        .map_err(sql)?;
    let mut found = Vec::new();
    for id in ids {
        if stmt.exists([id]).map_err(sql)? {
            found.push(id.clone());
        }
    }
    Ok(found)
}

/// The root `session` gets here: its parent's root when that parent is in
/// the import or the store, else (an orphan) itself.
fn root_of(
    tx: &Transaction<'_>,
    session: &Session,
    imported: &BTreeSet<SessionId>,
) -> Result<(Option<SessionId>, SessionId), Error> {
    let Some(parent) = session.parent_session_id else {
        return Ok((None, session.id));
    };
    if imported.contains(&parent) {
        let root: Option<String> = tx
            .query_row(
                "SELECT root_session_id FROM sessions WHERE id = ?1",
                [parent.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)?;
        if let Some(root) = root {
            return Ok((Some(parent), SessionId::parse(&root)?));
        }
    }
    match sessions::load(tx, parent)? {
        Some(row) => Ok((Some(parent), row.root_session_id)),
        None => Ok((None, session.id)),
    }
}

fn insert_session(
    tx: &Transaction<'_>,
    session: &Session,
    profile_id: &str,
    imported: &BTreeSet<SessionId>,
) -> Result<(), Error> {
    let (parent, root) = root_of(tx, session, imported)?;
    let title = match &session.title {
        Some(title) if crate::titles::in_use(tx, title)? => {
            Some(crate::titles::next_free(tx, title)?)
        }
        other => other.clone(),
    };
    let ms = |t: jiff::Timestamp| t.as_millisecond();
    tx.execute(
        "INSERT INTO sessions (id, profile_id, source, source_tag, origin, model, provider, cwd, \
         git_repo_root, git_branch, title, title_source, started_at, last_activity_at, ended_at, \
         end_reason, pinned, archived, hidden, bot_chat, parent_session_id, lineage_kind, \
         parent_deleted, root_session_id, compaction_count, rewind_count) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
         ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)",
        params![
            session.id.to_string(),
            profile_id,
            session.source.as_str(),
            session.source_tag,
            serde_json::to_string(&session.origin).expect("origins serialize"),
            session.model,
            session.provider,
            session.cwd,
            session.git_repo_root,
            session.git_branch,
            title,
            title
                .as_ref()
                .and(session.title_source.map(TitleSource::as_str)),
            ms(session.started_at),
            ms(session.last_activity_at),
            session.ended_at.map(ms),
            session.end_reason.map(EndReason::as_str),
            session.pinned,
            session.archived.as_str(),
            session.hidden,
            false,
            parent.map(|p| p.to_string()),
            session.lineage_kind.map(LineageKind::as_str),
            session.parent_deleted || (session.lineage_kind.is_some() && parent.is_none()),
            root.to_string(),
            session.compaction_count,
            session.rewind_count,
        ],
    )
    .map_err(sql)?;
    Ok(())
}

fn new_message(m: &Message) -> NewMessage {
    NewMessage {
        id: m.id,
        role: m.role,
        content: m.content.clone(),
        tool_calls: m.tool_calls.clone(),
        reasoning: m.reasoning.clone(),
        provider_payload: m.provider_payload.clone(),
        tool_call_id: m.tool_call_id.clone(),
        tool_name: m.tool_name.clone(),
        metadata: m.metadata.clone(),
    }
}

impl Store {
    /// Which of the given ids this store already has.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn import_collisions(
        &self,
        sessions: &[String],
        messages: &[String],
        calls: &[String],
    ) -> Result<Collisions, Error> {
        self.read(|conn| {
            Ok(Collisions {
                sessions: existing(conn, "sessions", "id", sessions)?,
                messages: existing(conn, "messages", "id", messages)?,
                calls: existing(conn, "usage_ledger", "call_id", calls)?,
            })
        })
    }

    /// The blobs `session`'s messages reference beyond their own spilled
    /// payloads (attachments), with the referencing message.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn message_blob_refs(
        &self,
        session: SessionId,
    ) -> Result<Vec<(MessageId, BlobRef)>, Error> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT r.owner_id, r.sha256 FROM blob_refs r JOIN messages m \
                     ON r.owner_kind = 'message' AND r.owner_id = m.id \
                     WHERE m.session_id = ?1 ORDER BY m.seq, r.sha256",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([session.to_string()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(sql)?;
            let mut out = Vec::new();
            for row in rows {
                let (message, sha) = row.map_err(sql)?;
                if let Some(blob) = BlobRef::parse(&format!("blob:sha256:{sha}")) {
                    out.push((MessageId::parse(&message)?, blob));
                }
            }
            Ok(out)
        })
    }

    /// Writes `sessions` (parents before children) in one transaction.
    /// Blob bytes must already be in the blob store
    /// ([`Store::put_blob`]). A title already in use gets the next free
    /// `#<n>`; a parent in neither the import nor the store leaves the
    /// session an orphan (`parent_deleted`).
    ///
    /// # Errors
    ///
    /// `store.constraint` (an id that exists: nothing is written),
    /// `store.io`, `store.sqlite`.
    pub fn import_portable(&self, sessions: &[Portable]) -> Result<(), Error> {
        self.ensure_writable()?;
        let blob_dir = self.location().blobs().to_path_buf();
        let redactor = self.redactor();
        let prepared = sessions
            .iter()
            .map(|p| {
                p.messages
                    .iter()
                    .map(|m| messages::prepare(&blob_dir, &new_message(m), redactor.as_ref()))
                    .collect::<Result<Vec<_>, _>>()
            })
            .collect::<Result<Vec<_>, _>>()?;
        let profile_id = self.profile_id().to_string();
        let profile = self.profile_name().to_owned();
        let imported: BTreeSet<SessionId> = sessions.iter().map(|p| p.session.id).collect();
        self.write(|tx| {
            for (portable, rows) in sessions.iter().zip(&prepared) {
                write_one(tx, portable, rows, (&profile_id, &profile), &imported)?;
            }
            Ok(())
        })
    }
}

fn write_one(
    tx: &Transaction<'_>,
    portable: &Portable,
    rows: &[messages::Prepared],
    (profile_id, profile): (&str, &str),
    imported: &BTreeSet<SessionId>,
) -> Result<(), Error> {
    let session = &portable.session;
    insert_session(tx, session, profile_id, imported)?;
    if !rows.is_empty() {
        messages::insert(tx, session.id, rows, 1)?;
    }
    for m in &portable.messages {
        tx.execute(
            "UPDATE messages SET created_at = ?2, active = ?3, compacted = ?4, \
             compaction_epoch = ?5 WHERE id = ?1",
            params![
                m.id.to_string(),
                m.created_at.as_millisecond(),
                m.active,
                m.compacted,
                m.compaction_epoch,
            ],
        )
        .map_err(sql)?;
    }
    tx.execute(
        "UPDATE sessions SET last_activity_at = ?2 WHERE id = ?1",
        params![
            session.id.to_string(),
            session.last_activity_at.as_millisecond()
        ],
    )
    .map_err(sql)?;
    for event in &portable.usage {
        crate::usage::insert(tx, profile, event)?;
    }
    for (message, blob) in &portable.blob_refs {
        tx.execute(
            "INSERT OR IGNORE INTO blob_refs (sha256, owner_kind, owner_id) \
             VALUES (?1, 'message', ?2)",
            params![blob.sha256(), message.to_string()],
        )
        .map_err(sql)?;
    }
    Ok(())
}
