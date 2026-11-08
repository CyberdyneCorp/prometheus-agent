//! `prometheus sessions import` (spec: session-storage, Portable export and
//! import): reads a `prometheus.sessions` JSONL export, checks all of it
//! (header, record shapes, bounds, references, id collisions) before
//! anything is written, then writes it in one store transaction.
//! `--remap-ids` gives every session, message and usage row a new id and
//! rewires parents and roots inside the file, so lineage survives.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use std::fmt::Write as _;

use base64::Engine as _;
use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_core::id::{MessageId, SessionId};
use pmt_store::portable::Portable;
use pmt_store::{BlobRef, Message, Session, Store, Tokens, UsageEvent};
use serde_json::Value;
use sha2::Digest as _;

/// Most sessions one file may hold.
pub const MAX_SESSIONS: usize = 500;
/// Most messages one file may hold.
pub const MAX_MESSAGES: usize = 50_000;
/// Largest file accepted.
pub const MAX_BYTES: u64 = 256 * 1024 * 1024;

/// What an import wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Imported {
    /// Session ids as stored (after any remap), in file order.
    pub sessions: Vec<String>,
    /// Messages written.
    pub messages: usize,
    /// Blobs written.
    pub blobs: usize,
}

fn invalid(line: usize, why: impl std::fmt::Display) -> Error {
    Error::new(
        &codes::IMPORT_INVALID,
        format!("line {line}: {why}; nothing was imported"),
    )
}

fn too_large(what: &str, limit: impl std::fmt::Display) -> Error {
    Error::new(
        &codes::IMPORT_TOO_LARGE,
        format!("the file holds more than {limit} {what}; nothing was imported"),
    )
    .with_hint("export fewer sessions per file")
}

/// The parsed file.
#[derive(Debug, Default)]
struct Parsed {
    sessions: Vec<Portable>,
    blobs: BTreeMap<String, Vec<u8>>,
    messages: usize,
}

impl Parsed {
    fn current(&mut self, line: usize, session: &str) -> Result<&mut Portable, Error> {
        self.sessions
            .iter_mut()
            .rev()
            .find(|p| p.session.id.to_string() == session)
            .ok_or_else(|| {
                invalid(
                    line,
                    format!("record of session {session} before its session record"),
                )
            })
    }
}

fn text(v: &Value, key: &str) -> Option<String> {
    v[key].as_str().map(str::to_owned)
}

fn time(v: &Value, key: &str) -> Result<Timestamp, String> {
    v[key]
        .as_str()
        .ok_or_else(|| format!("missing {key}"))?
        .parse()
        .map_err(|e| format!("{key}: {e}"))
}

fn opt_time(v: &Value, key: &str) -> Result<Option<Timestamp>, String> {
    if v[key].is_null() {
        return Ok(None);
    }
    time(v, key).map(Some)
}

fn parse<T: std::str::FromStr<Err = Error>>(v: &Value, key: &str) -> Result<T, String> {
    v[key]
        .as_str()
        .ok_or_else(|| format!("missing {key}"))?
        .parse()
        .map_err(|e: Error| e.message().to_owned())
}

fn opt_parse<T: std::str::FromStr<Err = Error>>(v: &Value, key: &str) -> Result<Option<T>, String> {
    if v[key].is_null() {
        return Ok(None);
    }
    parse(v, key).map(Some)
}

fn count(v: &Value, key: &str) -> u64 {
    v[key].as_u64().unwrap_or(0)
}

fn session(v: &Value) -> Result<Session, String> {
    Ok(Session {
        id: parse(v, "id")?,
        profile_id: String::new(),
        source: parse(v, "source")?,
        source_tag: text(v, "source_tag"),
        origin: serde_json::from_value(v["origin"].clone()).map_err(|e| format!("origin: {e}"))?,
        model: text(v, "model"),
        provider: text(v, "provider"),
        cwd: text(v, "cwd"),
        git_repo_root: text(v, "git_repo_root"),
        git_branch: text(v, "git_branch"),
        title: text(v, "title"),
        title_source: opt_parse(v, "title_source")?,
        started_at: time(v, "started_at")?,
        last_activity_at: time(v, "last_activity_at")?,
        ended_at: opt_time(v, "ended_at")?,
        end_reason: opt_parse(v, "end_reason")?,
        pinned: v["pinned"].as_bool().unwrap_or(false),
        archived: parse(v, "archived")?,
        hidden: v["hidden"].as_bool().unwrap_or(false),
        bot_chat: false,
        system_prompt_hash: None,
        parent_session_id: opt_parse(v, "parent_session_id")?,
        lineage_kind: opt_parse(v, "lineage_kind")?,
        parent_deleted: v["parent_deleted"].as_bool().unwrap_or(false),
        root_session_id: parse(v, "root_session_id")?,
        message_count: count(v, "message_count"),
        tool_call_count: count(v, "tool_call_count"),
        compaction_count: u32::try_from(count(v, "compaction_count")).unwrap_or(u32::MAX),
        rewind_count: u32::try_from(count(v, "rewind_count")).unwrap_or(u32::MAX),
        trajectory_purged_at: None,
    })
}

fn message(v: &Value) -> Result<Message, String> {
    Ok(Message {
        id: parse(v, "id")?,
        session_id: parse(v, "session_id")?,
        seq: count(v, "seq"),
        role: parse(v, "role")?,
        content: text(v, "content"),
        tool_calls: Some(v["tool_calls"].clone()).filter(|x| !x.is_null()),
        reasoning: Some(v["reasoning"].clone()).filter(|x| !x.is_null()),
        provider_payload: Some(v["provider_payload"].clone()).filter(|x| !x.is_null()),
        tool_call_id: text(v, "tool_call_id"),
        tool_name: text(v, "tool_name"),
        metadata: Some(v["metadata"].clone()).filter(|x| !x.is_null()),
        content_hash: String::new(),
        created_at: time(v, "created_at")?,
        active: v["active"].as_bool().unwrap_or(true),
        compacted: v["compacted"].as_bool().unwrap_or(false),
        compaction_epoch: None,
        content_ref: None,
    })
}

fn usage(v: &Value) -> Result<UsageEvent, String> {
    Ok(UsageEvent {
        call_id: text(v, "call_id").ok_or("missing call_id")?,
        ts: time(v, "ts")?,
        session_id: parse(v, "session_id")?,
        turn_id: text(v, "turn_id"),
        parent_call_id: text(v, "parent_call_id"),
        purpose: text(v, "purpose").ok_or("missing purpose")?,
        purpose_detail: text(v, "purpose_detail"),
        provider: text(v, "provider").ok_or("missing provider")?,
        model: text(v, "model").ok_or("missing model")?,
        service_tier: text(v, "service_tier"),
        tokens: Tokens {
            input: count(v, "input_tokens"),
            output: count(v, "output_tokens"),
            cache_read: count(v, "cache_read_tokens"),
            cache_write: count(v, "cache_write_tokens"),
            reasoning: count(v, "reasoning_tokens"),
        },
        cost_usd: v["cost_usd"].as_f64(),
        cost_status: parse(v, "cost_status")?,
        billing_source: text(v, "billing_source").ok_or("missing billing_source")?,
        request_hash: text(v, "request_hash"),
        cache_hit: v["cache_hit"].as_bool().unwrap_or(false),
    })
}

fn blob(v: &Value) -> Result<(String, Vec<u8>), String> {
    let sha = text(v, "sha256").ok_or("missing sha256")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(v["data"].as_str().ok_or("missing data")?)
        .map_err(|e| format!("data: {e}"))?;
    let reference = BlobRef::parse(&format!("blob:sha256:{sha}")).ok_or("bad sha256")?;
    let actual = sha2::Sha256::digest(&bytes)
        .iter()
        .fold(String::new(), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        });
    if actual != reference.sha256() {
        return Err(format!("blob {sha} does not match its data"));
    }
    Ok((sha, bytes))
}

/// Applies one record line to `parsed`.
fn record(parsed: &mut Parsed, line: usize, v: &Value) -> Result<(), Error> {
    let bad = |why: String| invalid(line, why);
    match v["type"].as_str() {
        Some("session") => {
            if parsed.sessions.len() == MAX_SESSIONS {
                return Err(too_large("sessions", MAX_SESSIONS));
            }
            parsed.sessions.push(Portable {
                session: session(&v["session"]).map_err(bad)?,
                messages: Vec::new(),
                usage: Vec::new(),
                blob_refs: Vec::new(),
                session_blobs: Vec::new(),
            });
        }
        Some("message") => {
            parsed.messages += 1;
            if parsed.messages > MAX_MESSAGES {
                return Err(too_large("messages", MAX_MESSAGES));
            }
            let m = message(&v["message"]).map_err(bad)?;
            parsed
                .current(line, &m.session_id.to_string())?
                .messages
                .push(m);
        }
        Some("usage") => {
            let u = usage(&v["usage"]).map_err(bad)?;
            parsed
                .current(line, &u.session_id.to_string())?
                .usage
                .push(u);
        }
        Some("blob") => {
            let (sha, bytes) = blob(v).map_err(bad)?;
            parsed.blobs.insert(sha, bytes);
        }
        Some("blob_ref") => {
            let message: Option<MessageId> = opt_parse(v, "message_id").map_err(bad)?;
            let sha = text(v, "sha256").unwrap_or_default();
            let reference = BlobRef::parse(&format!("blob:sha256:{sha}"))
                .ok_or_else(|| bad("bad sha256".into()))?;
            let session = text(v, "session_id").unwrap_or_default();
            let current = parsed.current(line, &session)?;
            match message {
                Some(message) => current.blob_refs.push((message, reference)),
                None => current.session_blobs.push(reference),
            }
        }
        Some("lineage") => {}
        other => return Err(bad(format!("unknown record type {other:?}"))),
    }
    Ok(())
}

fn read(path: &Path) -> Result<Parsed, Error> {
    let io = |e: std::io::Error| {
        Error::new(
            &codes::IMPORT_INVALID,
            format!("cannot read {}: {e}", path.display()),
        )
    };
    let size = std::fs::metadata(path).map_err(io)?.len();
    if size > MAX_BYTES {
        return Err(too_large("bytes", MAX_BYTES));
    }
    let file = std::fs::File::open(path).map_err(io)?;
    let mut lines = BufReader::new(file.take(MAX_BYTES + 1)).lines();
    let header: Value = lines
        .next()
        .ok_or_else(|| invalid(1, "empty file"))?
        .map_err(io)
        .and_then(|l| serde_json::from_str(&l).map_err(|e| invalid(1, e)))?;
    if header["format"] != "prometheus.sessions" || header["version"] != 1 {
        return Err(invalid(1, "not a prometheus.sessions version 1 export"));
    }
    let mut parsed = Parsed::default();
    for (index, line) in lines.enumerate() {
        let line_no = index + 2;
        let line = line.map_err(io)?;
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(&line).map_err(|e| invalid(line_no, e))?;
        record(&mut parsed, line_no, &v)?;
    }
    Ok(parsed)
}

/// Every referenced blob must be in the file or already in the store.
fn check_blobs(store: &Store, parsed: &Parsed) -> Result<(), Error> {
    for p in &parsed.sessions {
        for blob in p.blob_refs.iter().map(|(_, b)| b).chain(&p.session_blobs) {
            if !parsed.blobs.contains_key(blob.sha256()) && store.get_blob(blob).is_err() {
                return Err(Error::new(
                    &codes::IMPORT_INVALID,
                    format!(
                        "blob {} is referenced but neither in the file nor in this store; nothing was imported",
                        blob.sha256()
                    ),
                )
                .with_hint("export with --with-blobs"));
            }
        }
    }
    Ok(())
}

fn collisions(store: &Store, parsed: &Parsed) -> Result<(), Error> {
    let sessions: Vec<String> = parsed
        .sessions
        .iter()
        .map(|p| p.session.id.to_string())
        .collect();
    let messages: Vec<String> = parsed
        .sessions
        .iter()
        .flat_map(|p| p.messages.iter().map(|m| m.id.to_string()))
        .collect();
    let calls: Vec<String> = parsed
        .sessions
        .iter()
        .flat_map(|p| p.usage.iter().map(|u| u.call_id.clone()))
        .collect();
    let found = store.import_collisions(&sessions, &messages, &calls)?;
    if found.is_empty() {
        return Ok(());
    }
    let mut ids = found.sessions;
    ids.extend(found.messages);
    ids.extend(found.calls);
    Err(Error::new(
        &codes::IMPORT_ID_COLLISION,
        format!(
            "these ids already exist in this profile: {}; nothing was imported",
            ids.join(", ")
        ),
    )
    .with_hint("rerun with --remap-ids to import them under new ids"))
}

/// Gives every session, message and usage row a new id, keeping the
/// file's parent/root links.
fn remap(parsed: &mut Parsed) {
    let sessions: BTreeMap<SessionId, SessionId> = parsed
        .sessions
        .iter()
        .map(|p| (p.session.id, SessionId::new()))
        .collect();
    let to = |id: SessionId| sessions.get(&id).copied().unwrap_or(id);
    for p in &mut parsed.sessions {
        let messages: BTreeMap<MessageId, MessageId> = p
            .messages
            .iter()
            .map(|m| (m.id, MessageId::new()))
            .collect();
        p.session.id = to(p.session.id);
        p.session.parent_session_id = p.session.parent_session_id.map(to);
        p.session.root_session_id = to(p.session.root_session_id);
        for m in &mut p.messages {
            m.id = messages[&m.id];
            m.session_id = p.session.id;
        }
        for u in &mut p.usage {
            u.session_id = p.session.id;
            // Unique: the original call id is unique, the session is new.
            u.call_id = format!("{}-{}", u.call_id, p.session.id);
        }
        for (message, _) in &mut p.blob_refs {
            *message = messages.get(message).copied().unwrap_or(*message);
        }
    }
}

/// Parents before children: a session whose parent is in the file comes
/// after it.
fn order(parsed: &mut Parsed) -> Result<(), Error> {
    let mut placed: BTreeSet<SessionId> = BTreeSet::new();
    let ids: BTreeSet<SessionId> = parsed.sessions.iter().map(|p| p.session.id).collect();
    let mut left = std::mem::take(&mut parsed.sessions);
    while !left.is_empty() {
        let before = left.len();
        let (ready, rest): (Vec<_>, Vec<_>) = left.into_iter().partition(|p| {
            p.session
                .parent_session_id
                .is_none_or(|parent| !ids.contains(&parent) || placed.contains(&parent))
        });
        placed.extend(ready.iter().map(|p| p.session.id));
        parsed.sessions.extend(ready);
        left = rest;
        if left.len() == before {
            return Err(Error::new(
                &codes::IMPORT_INVALID,
                "the sessions' parent links form a cycle; nothing was imported",
            ));
        }
    }
    Ok(())
}

/// Imports `path` into `store`.
///
/// # Errors
///
/// `import.invalid`, `import.too_large`, `import.id_collision` (nothing
/// is written), store errors.
pub fn import(store: &Store, path: &Path, remap_ids: bool) -> Result<Imported, Error> {
    let mut parsed = read(path)?;
    let mut seen = BTreeSet::new();
    for p in &parsed.sessions {
        if !seen.insert(p.session.id) {
            return Err(Error::new(
                &codes::IMPORT_INVALID,
                format!(
                    "session {} appears twice; nothing was imported",
                    p.session.id
                ),
            ));
        }
    }
    check_blobs(store, &parsed)?;
    if remap_ids {
        remap(&mut parsed);
    } else {
        collisions(store, &parsed)?;
    }
    order(&mut parsed)?;
    for bytes in parsed.blobs.values() {
        store.put_blob(bytes)?;
    }
    store.import_portable(&parsed.sessions)?;
    Ok(Imported {
        sessions: parsed
            .sessions
            .iter()
            .map(|p| p.session.id.to_string())
            .collect(),
        messages: parsed.messages,
        blobs: parsed.blobs.len(),
    })
}
