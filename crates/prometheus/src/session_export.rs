//! Session export formats (spec: session-storage, Export formats and save
//! command; Portable export and import for the JSONL layout).
//!
//! Every format streams one session at a time to a writer, reading the
//! transcript row by row, so exports of any size work. Only active
//! messages are exported unless `include_inactive`. With a redactor, every
//! chunk is scrubbed (known secret values and credential-shaped text)
//! before it is written.

use std::fmt::Write as _;
use std::io::Write;

use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_core::secret::Redactor;
use pmt_store::{Message, Role, Session, Store, UsageEvent, Visibility};
use serde_json::{Value, json};

use crate::cli::ExportFormat;

/// What to export and how.
#[derive(Debug, Clone, Copy)]
pub struct ExportOptions<'a> {
    /// The output format.
    pub format: ExportFormat,
    /// Include rewound and compacted messages.
    pub include_inactive: bool,
    /// Scrub secrets before writing.
    pub redactor: Option<&'a Redactor>,
}

/// The file extension of a format.
#[must_use]
pub fn extension(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Jsonl | ExportFormat::Prompts | ExportFormat::Trace => "jsonl",
        ExportFormat::Md => "md",
        ExportFormat::Html => "html",
    }
}

/// A session record as JSON (listings, `show`, exports).
#[must_use]
pub fn session_json(session: &Session) -> Value {
    let ts = |t: jiff::Timestamp| t.to_string();
    json!({
        "id": session.id.to_string(),
        "title": session.title,
        "title_source": session.title_source.map(pmt_store::TitleSource::as_str),
        "source": session.source.as_str(),
        "source_tag": session.source_tag,
        "origin": session.origin,
        "model": session.model,
        "provider": session.provider,
        "cwd": session.cwd,
        "git_repo_root": session.git_repo_root,
        "git_branch": session.git_branch,
        "started_at": ts(session.started_at),
        "last_activity_at": ts(session.last_activity_at),
        "ended_at": session.ended_at.map(ts),
        "end_reason": session.end_reason.map(pmt_store::EndReason::as_str),
        "pinned": session.pinned,
        "archived": session.archived.as_str(),
        "hidden": session.hidden,
        "bot_chat": session.bot_chat,
        "parent_session_id": session.parent_session_id.map(|id| id.to_string()),
        "lineage_kind": session.lineage_kind.map(pmt_store::LineageKind::as_str),
        "parent_deleted": session.parent_deleted,
        "root_session_id": session.root_session_id.to_string(),
        "message_count": session.message_count,
        "tool_call_count": session.tool_call_count,
        "compaction_count": session.compaction_count,
        "rewind_count": session.rewind_count,
    })
}

fn message_json(message: &Message) -> Value {
    json!({
        "id": message.id.to_string(),
        "session_id": message.session_id.to_string(),
        "seq": message.seq,
        "role": message.role.as_str(),
        "content": message.content,
        "tool_calls": message.tool_calls,
        "reasoning": message.reasoning,
        "provider_payload": message.provider_payload,
        "tool_call_id": message.tool_call_id,
        "tool_name": message.tool_name,
        "metadata": message.metadata,
        "created_at": message.created_at.to_string(),
        "active": message.active,
        "compacted": message.compacted,
    })
}

fn usage_json(event: &UsageEvent) -> Value {
    json!({
        "call_id": event.call_id,
        "ts": event.ts.to_string(),
        "session_id": event.session_id.to_string(),
        "turn_id": event.turn_id,
        "parent_call_id": event.parent_call_id,
        "purpose": event.purpose,
        "purpose_detail": event.purpose_detail,
        "provider": event.provider,
        "model": event.model,
        "service_tier": event.service_tier,
        "input_tokens": event.tokens.input,
        "output_tokens": event.tokens.output,
        "cache_read_tokens": event.tokens.cache_read,
        "cache_write_tokens": event.tokens.cache_write,
        "reasoning_tokens": event.tokens.reasoning,
        "cost_usd": event.cost_usd,
        "cost_status": event.cost_status.as_str(),
        "billing_source": event.billing_source,
        "request_hash": event.request_hash,
        "cache_hit": event.cache_hit,
    })
}

/// Writes text chunks, redacted when asked.
struct Sink<'a> {
    out: &'a mut dyn Write,
    redactor: Option<&'a Redactor>,
}

impl Sink<'_> {
    fn put(&mut self, chunk: &str) -> Result<(), Error> {
        let written = match self.redactor {
            Some(redactor) => self.out.write_all(redactor.redact_all(chunk).as_bytes()),
            None => self.out.write_all(chunk.as_bytes()),
        };
        written.map_err(|e| Error::new(&codes::OUTPUT_IO, format!("cannot write the export: {e}")))
    }

    fn line(&mut self, value: &Value) -> Result<(), Error> {
        self.put(&format!("{value}\n"))
    }
}

/// Exports `sessions` to `out`.
///
/// # Errors
///
/// `session.not_found`, blob and SQLite errors, `output.io`.
pub fn export(
    store: &Store,
    sessions: &[SessionId],
    options: ExportOptions<'_>,
    out: &mut dyn Write,
) -> Result<(), Error> {
    let mut sink = Sink {
        out,
        redactor: options.redactor,
    };
    let visibility = if options.include_inactive {
        Visibility::All
    } else {
        Visibility::Active
    };
    match options.format {
        ExportFormat::Jsonl => {
            sink.line(&json!({"format": "prometheus.sessions", "version": 1}))?;
        }
        ExportFormat::Html => sink.put(HTML_HEAD)?,
        _ => {}
    }
    for id in sessions {
        let session = store.session(*id)?;
        match options.format {
            ExportFormat::Jsonl => jsonl(store, &session, visibility, &mut sink)?,
            ExportFormat::Md => markdown(store, &session, visibility, &mut sink)?,
            ExportFormat::Html => html(store, &session, visibility, &mut sink)?,
            ExportFormat::Prompts => prompts(store, &session, visibility, &mut sink)?,
            ExportFormat::Trace => trace(store, &session, visibility, &mut sink)?,
        }
    }
    if options.format == ExportFormat::Html {
        sink.put("</body>\n</html>\n")?;
    }
    Ok(())
}

fn jsonl(
    store: &Store,
    session: &Session,
    visibility: Visibility,
    sink: &mut Sink<'_>,
) -> Result<(), Error> {
    sink.line(&json!({"type": "session", "session": session_json(session)}))?;
    store.for_each_message(session.id, visibility, |m| {
        sink.line(&json!({"type": "message", "message": message_json(&m)}))
    })?;
    for event in store.usage_events(session.id)? {
        sink.line(&json!({"type": "usage", "usage": usage_json(&event)}))?;
    }
    sink.line(&json!({
        "type": "lineage",
        "session_id": session.id.to_string(),
        "parent_session_id": session.parent_session_id.map(|id| id.to_string()),
        "lineage_kind": session.lineage_kind.map(pmt_store::LineageKind::as_str),
        "root_session_id": session.root_session_id.to_string(),
    }))
}

fn title(session: &Session) -> String {
    session
        .title
        .clone()
        .unwrap_or_else(|| session.id.to_string())
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

fn markdown(
    store: &Store,
    session: &Session,
    visibility: Visibility,
    sink: &mut Sink<'_>,
) -> Result<(), Error> {
    sink.put(&format!(
        "# {}\n\n- id: `{}`\n- source: {}\n- started: {}\n\n",
        title(session),
        session.id,
        session.source,
        session.started_at
    ))?;
    store.for_each_message(session.id, visibility, |m| {
        let mut chunk = format!("## {} · {}\n\n", m.role, m.created_at);
        if let Some(text) = &m.content {
            if m.role == Role::Tool {
                let name = m.tool_name.as_deref().unwrap_or("tool");
                let _ = write!(chunk, "<details><summary>{name} result</summary>\n\n```\n{text}\n```\n\n</details>\n\n");
            } else {
                let _ = write!(chunk, "{text}\n\n");
            }
        }
        if let Some(calls) = &m.tool_calls {
            let _ = write!(chunk, "<details><summary>tool calls</summary>\n\n```json\n{}\n```\n\n</details>\n\n",
                pretty(calls));
        }
        sink.put(&chunk)
    })
}

const HTML_HEAD: &str = "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>Prometheus sessions</title>\n\
<style>body{font-family:system-ui,sans-serif;max-width:52rem;margin:2rem auto;padding:0 1rem;line-height:1.5}\
.msg{border-left:3px solid #999;padding:.25rem .75rem;margin:1rem 0}.user{border-color:#2563eb}\
.assistant{border-color:#16a34a}.tool{border-color:#a855f7}pre{white-space:pre-wrap;overflow-wrap:anywhere}\
.meta{color:#666;font-size:.85em}</style>\n</head>\n<body>\n";

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn html(
    store: &Store,
    session: &Session,
    visibility: Visibility,
    sink: &mut Sink<'_>,
) -> Result<(), Error> {
    sink.put(&format!(
        "<section>\n<h1>{}</h1>\n<p class=\"meta\">{} · {} · {}</p>\n",
        escape(&title(session)),
        session.id,
        session.source,
        session.started_at
    ))?;
    store.for_each_message(session.id, visibility, |m| {
        let mut chunk = format!(
            "<div class=\"msg {role}\"><p class=\"meta\">{role} · {}</p>\n",
            m.created_at,
            role = m.role
        );
        if let Some(text) = &m.content {
            let body = format!("<pre>{}</pre>", escape(text));
            if m.role == Role::Tool {
                let _ = writeln!(
                    chunk,
                    "<details><summary>tool result</summary>{body}</details>"
                );
            } else {
                chunk.push_str(&body);
                chunk.push('\n');
            }
        }
        if let Some(calls) = &m.tool_calls {
            let _ = writeln!(
                chunk,
                "<details><summary>tool calls</summary><pre>{}</pre></details>",
                escape(&pretty(calls))
            );
        }
        chunk.push_str("</div>\n");
        sink.put(&chunk)
    })?;
    sink.put("</section>\n")
}

fn prompts(
    store: &Store,
    session: &Session,
    visibility: Visibility,
    sink: &mut Sink<'_>,
) -> Result<(), Error> {
    store.for_each_message(session.id, visibility, |m| {
        if m.role != Role::User {
            return Ok(());
        }
        sink.line(&json!({
            "session_id": session.id.to_string(),
            "message_id": m.id.to_string(),
            "text": m.content,
        }))
    })
}

/// Trajectory events for one stored message.
fn trace_events(m: &Message) -> Vec<(&'static str, Value)> {
    let mut events = Vec::new();
    match m.role {
        Role::User => events.push((
            "user.message",
            json!({"message_id": m.id.to_string(), "text": m.content}),
        )),
        Role::Assistant => {
            if let Some(reasoning) = &m.reasoning {
                events.push((
                    "assistant.reasoning",
                    json!({"message_id": m.id.to_string(), "items": reasoning}),
                ));
            }
            events.push((
                "assistant.message",
                json!({"message_id": m.id.to_string(), "text": m.content}),
            ));
            for call in m
                .tool_calls
                .iter()
                .flat_map(|c| c.as_array().cloned().unwrap_or_default())
            {
                events.push((
                    "tool.call",
                    json!({"message_id": m.id.to_string(), "call": call}),
                ));
            }
        }
        Role::Tool => events.push((
            "tool.result",
            json!({"message_id": m.id.to_string(), "call_id": m.tool_call_id, "tool": m.tool_name,
                   "ok": true, "duration_ms": null, "output": m.content}),
        )),
        Role::System => {}
    }
    events
}

fn trace(
    store: &Store,
    session: &Session,
    visibility: Visibility,
    sink: &mut Sink<'_>,
) -> Result<(), Error> {
    let mut seq = 0;
    let mut emit = |sink: &mut Sink<'_>,
                    ts: jiff::Timestamp,
                    span: String,
                    kind: &'static str,
                    payload: Value| {
        seq += 1;
        let line = pmt_obs::trajectory::Line {
            v: pmt_obs::trajectory::version(),
            ts: pmt_obs::trajectory::timestamp(ts),
            seq,
            session_id: session.id.to_string(),
            root_session_id: session.root_session_id.to_string(),
            turn_id: None,
            span_id: span,
            parent_span_id: None,
            kind,
            payload,
        };
        sink.line(&serde_json::to_value(line).unwrap_or(Value::Null))
    };
    let start =
        json!({"source": session.source.as_str(), "title": session.title, "model": session.model});
    emit(
        sink,
        session.started_at,
        session.id.to_string(),
        "session.start",
        start,
    )?;
    store.for_each_message(session.id, visibility, |m| {
        for (kind, payload) in trace_events(&m) {
            emit(sink, m.created_at, m.id.to_string(), kind, payload)?;
        }
        Ok(())
    })?;
    if let (Some(ended), Some(reason)) = (session.ended_at, session.end_reason) {
        emit(
            sink,
            ended,
            session.id.to_string(),
            "session.end",
            json!({"reason": reason.as_str()}),
        )?;
    }
    Ok(())
}
