//! API session identity and ownership (spec: api-server, API session
//! identity and ownership; Caller instructions and long-term memory
//! scoping): the session headers, the caller's session key, and the rule
//! that the API server only takes turns in sessions it created.

use std::sync::Arc;

use pmt_store::{Source, Store};

use crate::http::{Reply, bad_request, conflict, not_found, with_header};

/// The session id header and its Hermes alias.
pub(crate) const SESSION_ID: [&str; 2] = ["x-prometheus-session-id", "x-hermes-session-id"];
/// The caller key header and its Hermes alias.
pub(crate) const SESSION_KEY: [&str; 2] = ["x-prometheus-session-key", "x-hermes-session-key"];
/// The longest session key.
const MAX_KEY_CHARS: usize = 256;

/// The caller's session key, checked: at most 256 characters and no
/// control characters.
pub(crate) fn session_key(req: &crate::http::Req) -> Result<Option<String>, Reply> {
    let Some(raw) = req
        .headers
        .get(SESSION_KEY[0])
        .or_else(|| req.headers.get(SESSION_KEY[1]))
    else {
        return Ok(None);
    };
    let Ok(key) = raw.to_str() else {
        return Err(bad_key());
    };
    if key.is_empty() || key.chars().count() > MAX_KEY_CHARS || key.chars().any(char::is_control) {
        return Err(bad_key());
    }
    Ok(Some(key.to_owned()))
}

fn bad_key() -> Reply {
    bad_request(
        "invalid_session_key",
        "X-Prometheus-Session-Key must be 1-256 characters without control characters",
    )
}

/// Session `id` of the profile when the API server owns it: 404 when it
/// does not exist, 409 `permission_denied` when another surface owns it.
pub(crate) fn owned(store: &Arc<Store>, id: &str) -> Result<pmt_store::Session, Reply> {
    let session = existing(store, id)?;
    if session.source == Source::Api {
        Ok(session)
    } else {
        Err(conflict(
            "permission_denied",
            &format!(
                "session {id} belongs to the {} surface; the API server only continues its own sessions",
                session.source.as_str()
            ),
        ))
    }
}

/// Session `id` of the profile, or 404.
pub(crate) fn existing(store: &Arc<Store>, id: &str) -> Result<pmt_store::Session, Reply> {
    id.parse()
        .ok()
        .and_then(|id| store.session(id).ok())
        .ok_or_else(|| not_found(&format!("session {id}")))
}

/// `reply` echoing the session id and caller key.
pub(crate) fn echo(reply: Reply, session: &str, key: Option<&str>) -> Reply {
    let reply = with_header(reply, SESSION_ID[0], session);
    match key {
        Some(key) => with_header(reply, SESSION_KEY[0], key),
        None => reply,
    }
}

/// The `prometheus` block SSE frames carry.
pub(crate) fn stamp(session: &str, key: Option<&str>) -> serde_json::Value {
    let mut stamp = serde_json::json!({"session_id": session});
    if let Some(key) = key {
        stamp["session_key"] = key.into();
    }
    stamp
}

/// The session a turn continues (`existing`, whose fixed instructions get
/// a notice when the caller now sends others) or a new API session.
pub(crate) async fn continue_or_create(
    shared: &crate::Shared,
    profile: &str,
    existing: Option<String>,
    new: crate::host::NewApiSession,
) -> Result<(String, Vec<String>), pmt_core::Error> {
    let Some(id) = existing else {
        let id = shared.host.api_create_session(profile, new).await?;
        return Ok((id, Vec::new()));
    };
    let fixed = match (shared.host.api_store(profile), id.parse()) {
        (Some(store), Ok(sid)) => store.api_session(sid)?.and_then(|s| s.instructions),
        _ => None,
    };
    let additions =
        crate::chat::changed_instructions(fixed.as_deref(), new.instructions.as_deref());
    Ok((id, additions))
}
