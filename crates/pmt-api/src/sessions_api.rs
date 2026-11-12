//! The Sessions REST API (spec: api-server, Sessions REST API). Reads
//! cover every session of the profile; chat, `PATCH` and `DELETE` only
//! the API's own sessions (another surface's answers 409
//! `permission_denied`). Session chat runs one turn synchronously or as
//! an event stream; its approvals wait for the session's other clients.

use std::sync::Arc;

use base64::Engine as _;
use hyper::Method;
use pmt_store::{EndReason, ListOptions, Source, TitleSource, Visibility};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::auth::Caller;
use crate::host::{ApiTurn, NewApiSession};
use crate::http::{Reply, Req, Sse, bad_request, from_error, json_reply, not_found, sse_reply};
use crate::turn::{self, Answering, Outcome, TurnEvent};
use crate::{Shared, content, model, session};

/// Routes `/api/sessions/...`.
pub(crate) async fn route(
    shared: &Arc<Shared>,
    caller: &Caller,
    rest: &[&str],
    req: &Req,
) -> Reply {
    let result = match (req.method.clone(), rest) {
        (Method::GET, [] | [""]) => list(shared, caller, req),
        (Method::POST, [] | [""]) => create(shared, caller, req).await,
        (Method::GET, [id]) => show(shared, caller, id),
        (Method::PATCH, [id]) => patch(shared, caller, id, req),
        (Method::DELETE, [id]) => delete(shared, caller, id),
        (Method::GET, [id, "messages"]) => messages(shared, caller, id, req),
        (Method::POST, [id, "fork"]) => fork(shared, caller, id, req).await,
        (Method::POST, [id, "chat"]) => chat(shared, caller, id, req, false).await,
        (Method::POST, [id, "chat", "stream"]) => chat(shared, caller, id, req, true).await,
        _ => Err(not_found("the route")),
    };
    match result {
        Ok(reply) | Err(reply) => reply,
    }
}

fn describe(session: &pmt_store::Session) -> Value {
    json!({
        "id": session.id.to_string(),
        "object": "session",
        "source": session.source.as_str(),
        "title": session.title,
        "model": session.model,
        "provider": session.provider,
        "started_at": session.started_at.to_string(),
        "last_activity_at": session.last_activity_at.to_string(),
        "ended_at": session.ended_at.map(|t| t.to_string()),
        "end_reason": session.end_reason.map(EndReason::as_str),
        "parent_session_id": session.parent_session_id.map(|p| p.to_string()),
        "lineage_kind": session.lineage_kind.map(pmt_store::LineageKind::as_str),
        "message_count": session.message_count,
    })
}

fn flag(req: &Req, name: &str, default: bool) -> bool {
    req.param(name)
        .map_or(default, |v| matches!(v.as_str(), "true" | "1" | "yes"))
}

fn list(shared: &Shared, caller: &Caller, req: &Req) -> Result<Reply, Reply> {
    let store = shared.store(&caller.profile)?;
    let limit = req
        .param("limit")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(50)
        .clamp(1, 200);
    let offset = req
        .param("offset")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let sources = match req.param("source") {
        Some(source) => vec![
            source
                .parse::<Source>()
                .map_err(|e| bad_request("invalid_source", e.message()))?,
        ],
        None => Vec::new(),
    };
    let children = flag(req, "include_children", false);
    let options = ListOptions {
        limit: None,
        sources,
        ..ListOptions::default()
    };
    let sessions: Vec<Value> = store
        .list_sessions(&options)
        .map_err(|e| from_error(&e))?
        .iter()
        .filter(|s| children || s.parent_session_id.is_none())
        .skip(offset)
        .take(limit as usize)
        .map(describe)
        .collect();
    Ok(json_reply(
        200,
        &json!({"object": "list", "data": sessions, "limit": limit, "offset": offset}),
    ))
}

async fn create(shared: &Shared, caller: &Caller, req: &Req) -> Result<Reply, Reply> {
    let body = req.json()?;
    let new = NewApiSession {
        instructions: body["instructions"].as_str().map(str::to_owned),
        session_key: session::session_key(req)?,
        title: body["title"].as_str().map(str::to_owned),
        ..NewApiSession::default()
    };
    let id = shared
        .host
        .api_create_session(&caller.profile, new)
        .await
        .map_err(|e| from_error(&e))?;
    show(shared, caller, &id).map(|reply| with_status(reply, 201))
}

fn with_status(mut reply: Reply, status: u16) -> Reply {
    if let Ok(status) = hyper::StatusCode::from_u16(status) {
        *reply.status_mut() = status;
    }
    reply
}

fn show(shared: &Shared, caller: &Caller, id: &str) -> Result<Reply, Reply> {
    let store = shared.store(&caller.profile)?;
    let found = session::existing(&store, id)?;
    Ok(json_reply(200, &describe(&found)))
}

fn patch(shared: &Shared, caller: &Caller, id: &str, req: &Req) -> Result<Reply, Reply> {
    let store = shared.store(&caller.profile)?;
    let found = session::owned(&store, id)?;
    let body = req.json()?;
    if let Some(title) = body.get("title") {
        store
            .set_title(found.id, title.as_str(), TitleSource::User)
            .map_err(|e| bad_request(e.code().code, e.message()))?;
    }
    if let Some(reason) = body["end_reason"].as_str() {
        let reason = reason
            .parse::<EndReason>()
            .map_err(|e| bad_request("invalid_end_reason", e.message()))?;
        store
            .end_session(found.id, reason)
            .map_err(|e| from_error(&e))?;
    }
    show(shared, caller, id)
}

fn delete(shared: &Shared, caller: &Caller, id: &str) -> Result<Reply, Reply> {
    let store = shared.store(&caller.profile)?;
    let found = session::owned(&store, id)?;
    store
        .delete_session(found.id, false)
        .map_err(|e| from_error(&e))?;
    Ok(json_reply(
        200,
        &json!({"id": id, "object": "session.deleted", "deleted": true}),
    ))
}

fn messages(shared: &Shared, caller: &Caller, id: &str, req: &Req) -> Result<Reply, Reply> {
    let store = shared.store(&caller.profile)?;
    let found = session::existing(&store, id)?;
    let compacted = flag(req, "include_compacted", false);
    let inline = flag(req, "inline_images", true);
    let visibility = if compacted {
        Visibility::All
    } else {
        Visibility::Active
    };
    let rows = store
        .messages(found.id, visibility)
        .map_err(|e| from_error(&e))?;
    let data: Vec<Value> = rows
        .iter()
        .filter(|m| compacted || !m.compacted)
        .map(|m| message(&store, m, inline))
        .collect();
    Ok(json_reply(
        200,
        &json!({"object": "list", "session_id": id, "data": data}),
    ))
}

/// One stored message; its images inline as data URLs, or as `[image]`.
fn message(store: &pmt_store::Store, row: &pmt_store::Message, inline: bool) -> Value {
    let metadata = row.metadata.as_ref();
    let mut text = metadata
        .and_then(|m| m["user_text"].as_str())
        .map(str::to_owned)
        .or_else(|| row.content.clone())
        .unwrap_or_default();
    let images: Vec<Value> = metadata
        .and_then(|m| m["images"].as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = json!({
        "id": row.id.to_string(),
        "role": row.role.as_str(),
        "created_at": row.created_at.to_string(),
        "compacted": row.compacted,
    });
    if let Some(calls) = &row.tool_calls {
        out["tool_calls"] = calls.clone();
    }
    if let Some(call) = &row.tool_call_id {
        out["tool_call_id"] = call.clone().into();
        out["tool_name"] = row.tool_name.clone().into();
    }
    if inline {
        let urls: Vec<Value> = images
            .iter()
            .filter_map(|image| data_url(store, image))
            .collect();
        if !urls.is_empty() {
            out["images"] = urls.into();
        }
    } else {
        for _ in &images {
            text.push_str("\n[image]");
        }
    }
    out["content"] = text.into();
    out
}

fn data_url(store: &pmt_store::Store, image: &Value) -> Option<Value> {
    let blob = pmt_store::BlobRef::parse(image["blob"].as_str()?)?;
    let bytes = store.get_blob(&blob).ok()?;
    let media = image["media_type"].as_str().unwrap_or("image/png");
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Some(
        json!({"type": "image_url", "image_url": {"url": format!("data:{media};base64,{encoded}")}}),
    )
}

async fn fork(shared: &Shared, caller: &Caller, id: &str, req: &Req) -> Result<Reply, Reply> {
    let store = shared.store(&caller.profile)?;
    let parent = session::existing(&store, id)?;
    let body = req.json()?;
    let params = pmt_rpc::contracts::ForkParams {
        session_id: parent.id.to_string(),
        at_message_id: body["at_message_id"].as_str().map(str::to_owned),
        title: body["title"].as_str().map(str::to_owned),
    };
    let forked = shared
        .host
        .fork(&caller.profile, params)
        .await
        .map_err(|e| from_error(&e))?;
    if let (Ok(Some(row)), Ok(child)) = (store.api_session(parent.id), forked.session_id.parse()) {
        let copy = pmt_store::api::ApiSession {
            session_id: child,
            instructions: row.instructions,
            session_key: row.session_key,
            derived_key: None,
        };
        if let Err(err) = store.put_api_session(&copy) {
            tracing::warn!("fork's caller instructions not kept: {err}");
        }
    }
    show(shared, caller, &forked.session_id).map(|reply| with_status(reply, 201))
}

async fn chat(
    shared: &Arc<Shared>,
    caller: &Caller,
    id: &str,
    req: &Req,
    stream: bool,
) -> Result<Reply, Reply> {
    let store = shared.store(&caller.profile)?;
    session::owned(&store, id)?;
    let body = req.json()?;
    let given = body
        .get("message")
        .or_else(|| body.get("input"))
        .cloned()
        .unwrap_or(Value::Null);
    let input = content::user_input(&given)?;
    if input.text.trim().is_empty() && input.images.is_empty() {
        return Err(bad_request("missing_message", "message must not be empty"));
    }
    let config = shared.scope(&caller.profile)?.config().api_server.clone();
    let selected = model::select(shared, caller, &body, Some(id), false).await?;
    let fixed = id
        .parse()
        .ok()
        .and_then(|sid| store.api_session(sid).ok().flatten())
        .and_then(|s| s.instructions);
    let permit = shared
        .limits
        .acquire(&caller.profile, config.max_concurrent_runs)?;
    let turn = ApiTurn {
        session_id: id.to_owned(),
        text: input.text,
        images: input.images,
        additions: crate::chat::changed_instructions(
            fixed.as_deref(),
            body["instructions"].as_str(),
        ),
        model: selected.choice,
        options: selected.options,
        unattended: None,
    };
    if !stream {
        let (tx, _rx) = mpsc::unbounded_channel();
        let outcome = turn::run(shared, &caller.profile, turn, &tx, Answering::Others)
            .await
            .map_err(|e| from_error(&e))?;
        drop(permit);
        return Ok(json_reply(200, &result(id, &outcome)));
    }
    let (out, reply) = sse_reply();
    let shared = Arc::clone(shared);
    let profile = caller.profile.clone();
    let session_id = id.to_owned();
    pmt_core::spawn::spawn(async move {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let work = turn::run(&shared, &profile, turn, &tx, Answering::Others);
        tokio::pin!(work);
        let outcome = loop {
            tokio::select! {
                outcome = &mut work => break outcome,
                Some(event) = rx.recv() => relay(&shared, &profile, &out, event),
            }
        };
        while let Ok(event) = rx.try_recv() {
            relay(&shared, &profile, &out, event);
        }
        let (name, data) = match outcome {
            Ok(outcome) => {
                let data = result(&session_id, &outcome);
                let name = match data["status"].as_str() {
                    Some("completed") => "run.completed",
                    Some("cancelled") => "run.cancelled",
                    _ => "run.failed",
                };
                (name, data)
            }
            Err(err) => (
                "run.failed",
                json!({"status": "failed", "error": err.message()}),
            ),
        };
        let _ = out.send(Sse::named(name, &data));
        drop(permit);
    });
    Ok(reply)
}

fn relay(shared: &Shared, profile: &str, out: &mpsc::UnboundedSender<Sse>, event: TurnEvent) {
    let preview = |text: &str| crate::runs::redacted_preview(shared, profile, text);
    let (name, data) = match event {
        TurnEvent::Delta(text) => ("assistant.delta", json!({"delta": text})),
        TurnEvent::Commentary(text) => ("assistant.commentary", json!({"text": text})),
        TurnEvent::ToolStarted {
            call_id,
            tool,
            arguments,
        } => (
            "tool.started",
            json!({"tool": tool, "call_id": call_id, "preview": preview(&arguments.to_string())}),
        ),
        TurnEvent::ToolCompleted {
            call_id,
            tool,
            ok,
            duration_ms,
            output,
        } => (
            if ok { "tool.completed" } else { "tool.failed" },
            json!({"tool": tool, "call_id": call_id, "duration_ms": duration_ms,
                "preview": preview(&output)}),
        ),
        _ => return,
    };
    let _ = out.send(Sse::named(name, &data));
}

fn result(session: &str, outcome: &Outcome) -> Value {
    let status = match outcome.status() {
        "incomplete" => "completed",
        other => other,
    };
    let mut result = json!({
        "session_id": session,
        "status": status,
        "reply": outcome.final_response,
        "usage": outcome.openai_usage(),
    });
    if status != "completed" {
        result["error"] = outcome.error_text().into();
    }
    if let Some(runtime) = &outcome.runtime {
        result["runtime"] = json!({"provider": runtime.provider, "model": runtime.model});
    }
    result
}
