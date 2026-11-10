//! The gateway's hub connection: it attaches to every session it routes
//! (as a driver that answers server requests) and hands each event and
//! server request of a session to that session's conversation lane.

use std::sync::Arc;

use pmt_rpc::frame::{self, Id, Incoming};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_tungstenite::tungstenite::Message;

use super::Shared;
use super::lane::Msg;

/// Attaches the gateway to `session_id` of `profile`, so its events and
/// prompts reach the conversation of `key`.
pub(crate) fn attach(shared: &Shared, profile: &str, session_id: &str, key: &str) {
    shared
        .sessions
        .lock()
        .expect("sessions poisoned")
        .insert(session_id.to_owned(), key.to_owned());
    let hub = shared.server.hub();
    hub.register(session_id, profile);
    if !hub.is_attached(shared.conn, session_id) {
        hub.attach(shared.conn, session_id, shared.peer.clone(), None);
    }
}

/// Attaches the gateway to `session_id` of `profile` without routing it to
/// a conversation (a background task's session: only its end matters).
pub(crate) fn watch(shared: &Shared, profile: &str, session_id: &str) {
    let hub = shared.server.hub();
    hub.register(session_id, profile);
    if !hub.is_attached(shared.conn, session_id) {
        hub.attach(shared.conn, session_id, shared.peer.clone(), None);
    }
}

/// Reads the gateway's frames until the hub drops it.
pub(crate) async fn read(shared: Arc<Shared>, mut rx: UnboundedReceiver<Message>) {
    while let Some(message) = rx.recv().await {
        let Message::Text(text) = message else {
            continue;
        };
        match frame::parse(text.as_str()) {
            Ok(Incoming::Notification { method, params }) => event(&shared, &method, &params),
            Ok(Incoming::Request {
                id, method, params, ..
            }) => request(&shared, id, &method, params),
            Ok(Incoming::Response { .. }) | Err(_) => {}
        }
    }
}

fn lane_of(shared: &Shared, session_id: &str) -> Option<super::lane::Handle> {
    let key = shared
        .sessions
        .lock()
        .expect("sessions poisoned")
        .get(session_id)
        .cloned()?;
    shared
        .lanes
        .lock()
        .expect("lanes poisoned")
        .get(&key)
        .cloned()
}

fn event(shared: &Shared, method: &str, envelope: &Value) {
    let Some(session) = envelope["session_id"].as_str() else {
        return;
    };
    if method == "turn.finished" && shared.tasks.owns(session) {
        if let Ok(finished) = serde_json::from_value(envelope["data"].clone()) {
            super::tasks::finished(shared, session, &finished);
        }
        return;
    }
    let Some(lane) = lane_of(shared, session) else {
        return;
    };
    let data = envelope["data"].clone();
    let msg = match method {
        "turn.finished" => match serde_json::from_value(data) {
            Ok(finished) => Msg::Finished(finished),
            Err(_) => return,
        },
        "request.resolved" => Msg::Resolved {
            id: data["id"].as_str().unwrap_or_default().to_owned(),
            by_client: data["by_client"].as_str().unwrap_or_default().to_owned(),
        },
        "request.cancel" => Msg::Cancelled {
            id: data["id"].as_str().unwrap_or_default().to_owned(),
            reason: data["reason"].as_str().unwrap_or_default().to_owned(),
        },
        "session.event" => Msg::Event {
            kind: data["type"].as_str().unwrap_or_default().to_owned(),
            payload: data["payload"].clone(),
        },
        _ => return,
    };
    lane.send(msg);
}

fn request(shared: &Shared, id: Id, method: &str, params: Value) {
    let Id::Str(id) = id else {
        return;
    };
    let Some(lane) = params["session_id"]
        .as_str()
        .and_then(|session| lane_of(shared, session))
    else {
        return;
    };
    lane.send(Msg::Asked {
        id,
        method: method.to_owned(),
        params,
    });
}
