//! WebSocket upgrades (spec: web-dashboard, Structured chat over the client
//! protocol; Live event stream; CSRF protection on state-changing
//! requests; Same-origin only; Host header validation).
//!
//! `/api/rpc` is the client protocol itself: once admitted, the socket is
//! handed to the gateway's connection loop with a grant of every served
//! profile (each call names its profile), as an observer-only connection
//! for observers. `/api/events` is the daemon event stream. An upgrade is
//! admitted only with a valid Host, the dashboard's own Origin, a session
//! cookie and the session's CSRF token as a `csrf.<token>` subprotocol; a
//! refused upgrade completes and is closed at once (4403 not allowed, 4401
//! not signed in, 4404 unknown endpoint), so browsers see the code.

use std::sync::Arc;

use futures_util::{SinkExt as _, StreamExt as _};
use hyper::body::Incoming;
use hyper::header::HeaderValue;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use pmt_gateway::connection::Transport;
use pmt_gateway::tickets::Grant;
use pmt_rpc::codes::CloseCode;
use pmt_rpc::control::Purpose;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};

use crate::Shared;
use crate::auth::sessions::{Role as SessionRole, Signed, Via, csrf_ok};
use crate::events::{Event, Replay};
use crate::http::{Reply, Req, empty, error};

/// The event stream's subprotocol.
const EVENTS_PROTOCOL: &str = "prometheus-events";

/// Whether `request` asks for a WebSocket.
pub(crate) fn is_upgrade(request: &Request<Incoming>) -> bool {
    request
        .headers()
        .get(hyper::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

fn offered(req: &Req) -> Vec<String> {
    req.headers
        .get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Where an admitted socket goes.
enum Endpoint {
    Rpc(Signed),
    Events {
        profiles: Vec<String>,
        since: Option<u64>,
        signed: Signed,
    },
}

/// The admission of an upgrade: where it goes, or the close code.
fn admit(
    shared: &Shared,
    req: &Req,
    host_ok: bool,
    offered: &[String],
) -> Result<Endpoint, CloseCode> {
    if !host_ok {
        return Err(CloseCode::NotAllowed);
    }
    let path = req.path.as_str();
    if path != "/api/rpc" && path != "/api/events" {
        return Err(CloseCode::EndpointDisabled);
    }
    if let (Some(origin), Some(host)) = (req.header("origin"), req.header("host"))
        && origin != shared.guard.origin(host)
    {
        tracing::warn!(origin, "refused a dashboard WebSocket from another origin");
        return Err(CloseCode::NotAllowed);
    }
    let Some(caller) = crate::api::caller(shared, req) else {
        return Err(CloseCode::BadCredential);
    };
    let signed = caller.signed;
    let csrf = offered.iter().find_map(|p| p.strip_prefix("csrf."));
    if signed.via == Via::Cookie && !csrf_ok(&signed, csrf) {
        return Err(CloseCode::NotAllowed);
    }
    if path == "/api/rpc" {
        return Ok(Endpoint::Rpc(signed));
    }
    let served: Vec<String> = shared
        .server
        .profiles()
        .into_iter()
        .filter(|p| signed.may_use(p))
        .collect();
    let profiles: Vec<String> = match req.param("profiles") {
        Some(list) => list.split(',').map(str::to_owned).collect(),
        None => served.clone(),
    };
    if profiles.iter().any(|p| !served.contains(p)) {
        return Err(CloseCode::NotAllowed);
    }
    let since = req.param("since").and_then(|s| s.parse().ok());
    Ok(Endpoint::Events {
        profiles,
        since,
        signed,
    })
}

/// Answers an upgrade request: `101` and a task that serves (or closes)
/// the socket.
pub(crate) fn upgrade(
    shared: &Arc<Shared>,
    request: Request<Incoming>,
    req: &Req,
    host_ok: bool,
) -> Reply {
    let Some(key) = req.header("sec-websocket-key") else {
        return error(400, "bad_upgrade", "missing Sec-WebSocket-Key");
    };
    let offered = offered(req);
    let echoed = offered
        .iter()
        .find(|p| p.starts_with("prometheus-rpc.v") || *p == EVENTS_PROTOCOL)
        .cloned();
    let admission = admit(shared, req, host_ok, &offered);
    let mut reply = empty(101);
    *reply.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let headers = reply.headers_mut();
    headers.insert(
        hyper::header::UPGRADE,
        HeaderValue::from_static("websocket"),
    );
    headers.insert(
        hyper::header::CONNECTION,
        HeaderValue::from_static("Upgrade"),
    );
    if let Ok(accept) = HeaderValue::from_str(&derive_accept_key(key.as_bytes())) {
        headers.insert("sec-websocket-accept", accept);
    }
    if let Some(value) = echoed.and_then(|p| HeaderValue::from_str(&p).ok()) {
        headers.insert("sec-websocket-protocol", value);
    }
    // An event stream subscribes before its `101` is sent: whatever
    // happens once the client sees the socket open reaches it.
    let followed = match &admission {
        Ok(Endpoint::Events { since, .. }) => Some(shared.feed.follow(*since)),
        _ => None,
    };
    let shared = Arc::clone(shared);
    pmt_core::spawn::spawn(async move {
        let Ok(upgraded) = hyper::upgrade::on(request).await else {
            return;
        };
        let max = usize::try_from(shared.server.rpc_config().max_frame_bytes).ok();
        let config = WebSocketConfig::default()
            .max_message_size(max)
            .max_frame_size(max);
        let socket =
            WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, Some(config))
                .await;
        match admission {
            Err(code) => refuse(socket, code).await,
            Ok(Endpoint::Rpc(signed)) => rpc(&shared, socket, &signed).await,
            Ok(Endpoint::Events {
                profiles, signed, ..
            }) => {
                if let Some(followed) = followed {
                    events(&shared, socket, &profiles, followed, &signed).await;
                }
            }
        }
    });
    reply
}

fn close_message(code: CloseCode) -> Message {
    Message::Close(Some(CloseFrame {
        code: code.code().into(),
        reason: code.reason().into(),
    }))
}

async fn refuse<S: Transport>(mut socket: WebSocketStream<S>, code: CloseCode) {
    let wait = std::time::Duration::from_secs(1);
    let _ = tokio::time::timeout(wait, socket.send(close_message(code))).await;
    let _ = tokio::time::timeout(wait, socket.close(None)).await;
}

/// The client protocol, as the gateway serves any client.
async fn rpc<S: Transport>(shared: &Arc<Shared>, socket: WebSocketStream<S>, signed: &Signed) {
    let grant = Grant {
        profiles: shared
            .server
            .profiles()
            .into_iter()
            .filter(|p| signed.may_use(p))
            .collect(),
        purpose: Purpose::Http,
        pid: None,
        observer_only: signed.role == SessionRole::Observer,
        admin: signed.admin,
    };
    let ends = pmt_gateway::connection::Ends {
        stop: shared.stop.clone(),
        revoked: signed.ended.clone(),
    };
    pmt_gateway::connection::serve_upgraded(Arc::clone(&shared.server), socket, grant, ends).await;
}

/// The daemon event stream for `profiles`.
async fn events<S: Transport>(
    shared: &Arc<Shared>,
    mut socket: WebSocketStream<S>,
    profiles: &[String],
    (replay, mut live): (Replay, broadcast::Receiver<Event>),
    signed: &Signed,
) {
    let mine = |profile: &str| profiles.iter().any(|p| p == profile);
    if replay.truncated {
        let _ = socket
            .send(Message::text(json!({"truncated": true}).to_string()))
            .await;
    }
    for event in replay.events.iter().filter(|e| mine(&e.profile)) {
        if socket
            .send(Message::text(event.frame.to_string()))
            .await
            .is_err()
        {
            return;
        }
    }
    let closing = shared.server.closing();
    loop {
        let outgoing = tokio::select! {
            () = shared.stop.cancelled() => Some(close_message(CloseCode::ShuttingDown)),
            () = closing.cancelled() => Some(close_message(CloseCode::ShuttingDown)),
            () = signed.ended.cancelled() => Some(close_message(CloseCode::BadCredential)),
            incoming = socket.next() => match incoming {
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => None,
            },
            event = live.recv() => match event {
                Ok(event) if mine(&event.profile) => Some(Message::text(event.frame.to_string())),
                Ok(_) => None,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    Some(Message::text(json!({"truncated": true}).to_string()))
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
        };
        let Some(message) = outgoing else { continue };
        let closing_now = matches!(message, Message::Close(_));
        if socket.send(message).await.is_err() || closing_now {
            let _ = socket.close(None).await;
            return;
        }
    }
}
