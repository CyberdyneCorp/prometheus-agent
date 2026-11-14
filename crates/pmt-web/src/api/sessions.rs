//! A profile's sessions (spec: web-dashboard, Profile-addressed routes):
//! the paged listing with full-text search (`q`), a transcript, and
//! deletion, which touches only the addressed profile's `state.db`.

use pmt_core::id::SessionId;
use pmt_store::{ListOptions, SearchOptions, Visibility};
use serde_json::{Value, json};

use super::profile::Profile;
use crate::Shared;
use crate::http::{Reply, Req, error, from_error, json_reply};

/// The most sessions one page lists.
const MAX_PAGE: u32 = 200;

fn describe(session: &pmt_store::Session) -> Value {
    json!({
        "id": session.id.to_string(),
        "source": session.source.as_str(),
        "title": session.title,
        "model": session.model,
        "provider": session.provider,
        "started_at": session.started_at.to_string(),
        "last_activity_at": session.last_activity_at.to_string(),
        "ended_at": session.ended_at.map(|t| t.to_string()),
        "parent_session_id": session.parent_session_id.map(|p| p.to_string()),
        "message_count": session.message_count,
    })
}

fn page(req: &Req) -> (u32, u32) {
    let limit = req
        .param("limit")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(50)
        .clamp(1, MAX_PAGE);
    let offset = req
        .param("offset")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    (limit, offset)
}

/// `GET sessions[?q=&limit=&offset=]`.
pub(crate) fn list(at: &Profile<'_>, req: &Req) -> Reply {
    let (limit, offset) = page(req);
    if let Some(query) = req.param("q").filter(|q| !q.trim().is_empty()) {
        let options = SearchOptions {
            limit,
            offset,
            ..SearchOptions::default()
        };
        return match at.store.search(&query, &options) {
            Ok(results) => {
                let hits: Vec<Value> = results
                    .hits
                    .iter()
                    .map(|hit| {
                        json!({"session_id": hit.session_id.to_string(), "title": hit.session_title,
                               "message_id": hit.message_id.to_string(), "snippet": hit.snippet})
                    })
                    .collect();
                json_reply(
                    200,
                    &json!({"hits": hits, "limit": limit, "offset": offset}),
                )
            }
            Err(err) => from_error(&err),
        };
    }
    match at.store.list_sessions(&ListOptions::default()) {
        Ok(sessions) => {
            let total = sessions.len();
            let rows: Vec<Value> = sessions
                .iter()
                .skip(offset as usize)
                .take(limit as usize)
                .map(describe)
                .collect();
            json_reply(
                200,
                &json!({"sessions": rows, "total": total, "limit": limit, "offset": offset}),
            )
        }
        Err(err) => from_error(&err),
    }
}

fn session_id(id: &str) -> Result<SessionId, Reply> {
    SessionId::parse(id).map_err(|_| error(404, "not_found", &format!("no session {id}")))
}

/// `GET sessions/{id}/messages`: the active transcript.
pub(crate) fn messages(at: &Profile<'_>, id: &str) -> Reply {
    let id = match session_id(id) {
        Ok(id) => id,
        Err(reply) => return reply,
    };
    if at.store.session(id).is_err() {
        return error(
            404,
            "not_found",
            &format!("no session {id} in profile {}", at.name),
        );
    }
    match at.store.messages(id, Visibility::Active) {
        Ok(messages) => {
            let rows: Vec<Value> = messages
                .iter()
                .map(|m| {
                    json!({"id": m.id.to_string(), "seq": m.seq, "role": m.role.as_str(),
                           "content": m.content, "tool_calls": m.tool_calls,
                           "tool_name": m.tool_name, "created_at": m.created_at.to_string()})
                })
                .collect();
            json_reply(
                200,
                &json!({"session_id": id.to_string(), "messages": rows}),
            )
        }
        Err(err) => from_error(&err),
    }
}

/// `DELETE sessions/{id}`: refused while a client has it open.
pub(crate) fn delete(shared: &Shared, at: &Profile<'_>, id: &str) -> Reply {
    let id = match session_id(id) {
        Ok(id) => id,
        Err(reply) => return reply,
    };
    if at.store.session(id).is_err() {
        return error(
            404,
            "not_found",
            &format!("no session {id} in profile {}", at.name),
        );
    }
    if shared.server.hub().has_clients(&id.to_string()) {
        return error(
            409,
            "session_open",
            "the session is open in a client; close it first",
        );
    }
    match at.store.delete_session(id, false) {
        Ok(_) => json_reply(200, &json!({"deleted": id.to_string()})),
        Err(err) => from_error(&err),
    }
}

/// `GET sessions/{id}/file?path=`: a host file of the session, downloaded
/// only through the session's file-access rules (spec: web-dashboard,
/// Webapp file staging and terminals); the daemon applies them.
pub(crate) async fn host_file(shared: &Shared, profile: &str, id: &str, req: &Req) -> Reply {
    let Some(path) = req.param("path").filter(|p| !p.is_empty()) else {
        return error(400, "invalid_query", "path= is required");
    };
    match shared.host.session_file(profile, id, &path).await {
        Ok(file) => {
            let mut reply = crate::http::bytes(200, "application/octet-stream", file.bytes);
            let name: String = file
                .name
                .chars()
                .map(|c| {
                    if c.is_ascii_graphic() && c != '"' && c != '\\' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            if let Ok(value) =
                hyper::header::HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))
            {
                reply
                    .headers_mut()
                    .insert(hyper::header::CONTENT_DISPOSITION, value);
            }
            reply
        }
        Err(err) => from_error(&err),
    }
}
