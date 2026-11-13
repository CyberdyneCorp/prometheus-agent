//! One client connection (spec: workspace-architecture, Ticket-authenticated
//! loopback attachment; JSON-RPC framing and error codes; Protocol handshake
//! and versioning; Connection liveness and backpressure).
//!
//! The upgrade is admitted only with a loopback `Host`, an allowed (or no)
//! `Origin`, a `prometheus-rpc.v<major>` subprotocol and a valid single-use
//! `prometheus-ticket.<ticket>` subprotocol; a refused upgrade completes and
//! is closed at once with 4403 or 4401, so browsers see the close code.
//! An upgrade to any path but `/rpc` is closed with 4404 before its
//! credential is looked at. Then `rpc.hello` must come first; `rpc.ping` is
//! answered from the read loop itself; requests the catalog marks `long`
//! run as their own tasks on the bounded worker pool and may answer out of
//! order, every other request is answered in arrival order; a handler that
//! panics answers `-32099` and the connection stays open; a send blocked
//! for 30 s or 45 s without any frame ends the connection (spec: tui,
//! Client protocol framing; Long requests do not block the connection;
//! Gateway retirement refuses new work).

use std::ops::ControlFlow;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt as _, StreamExt as _};
use pmt_core::error::{Error, codes};
use pmt_rpc::codes::CloseCode;
use pmt_rpc::contracts::HelloParams;
use pmt_rpc::frame::{self, Id, Incoming, RpcError};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::server::{Call, Conn, HelloState, Server};
use crate::tickets::random_hex;
use crate::tickets::{Grant, Redeemed};

/// The longest a single send may block.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// Silence after which the daemon closes with 4408.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(45);

/// The path of the client protocol endpoint; other paths are not served on
/// this listener.
pub const RPC_PATH: &str = "/rpc";

/// Methods a retiring daemon still answers: everything else is new work.
/// `approval.respond` lets draining turns finish; the rest only read.
const WHILE_RETIRING: &[&str] = &[
    "approval.respond",
    "session.detach",
    "session.events.since",
    "session.list",
    "session.transcript",
    "session.context_usage",
];

/// The upgrade decision.
#[derive(Debug)]
enum Admission {
    Accepted(Grant),
    Refused(CloseCode),
}

type Socket = WebSocketStream<TcpStream>;

/// What a WebSocket can run over: the daemon's own TCP listener, or a
/// connection another listener (the dashboard's) upgraded.
pub trait Transport: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static {}

impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static> Transport for T {}

/// Serves one accepted TCP connection until it closes.
// The upgrade callback's error type is fixed by tungstenite.
#[allow(clippy::result_large_err)]
pub async fn serve(server: Arc<Server>, stream: TcpStream) {
    let mut admission = None;
    let config = WebSocketConfig::default()
        .max_message_size(usize::try_from(server.rpc_config().max_frame_bytes).ok())
        .max_frame_size(usize::try_from(server.rpc_config().max_frame_bytes).ok());
    let callback =
        |request: &Request, response: Response| upgrade(&server, request, response, &mut admission);
    let Ok(socket) =
        tokio_tungstenite::accept_hdr_async_with_config(stream, callback, Some(config)).await
    else {
        return;
    };
    match admission {
        Some(Admission::Accepted(grant)) => {
            run(server, socket, grant, Ends::default()).await;
        }
        Some(Admission::Refused(code)) => refuse(socket, code).await,
        None => {}
    }
}

/// Checks the upgrade request and picks the subprotocol to echo.
#[allow(clippy::result_large_err)] // tungstenite's callback signature
fn upgrade(
    server: &Server,
    request: &Request,
    mut response: Response,
    admission: &mut Option<Admission>,
) -> Result<Response, ErrorResponse> {
    let offered = offered_protocols(request);
    if request.uri().path() != RPC_PATH {
        // Upgraded and closed, so a browser sees the code; the ticket is
        // never redeemed.
        if let Some(version) = offered.iter().find(|p| p.starts_with("prometheus-rpc.v"))
            && let Ok(value) = HeaderValue::from_str(version)
        {
            response
                .headers_mut()
                .insert("Sec-WebSocket-Protocol", value);
        }
        *admission = Some(Admission::Refused(CloseCode::EndpointDisabled));
        return Ok(response);
    }
    let Some(version) = offered.iter().find(|p| p.starts_with("prometheus-rpc.v")) else {
        let mut refusal =
            ErrorResponse::new(Some("expected subprotocol prometheus-rpc.v<major>".into()));
        *refusal.status_mut() = StatusCode::BAD_REQUEST;
        return Err(refusal);
    };
    if let Ok(value) = HeaderValue::from_str(version) {
        response
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", value);
    }
    *admission = Some(admit(server, request, &offered));
    Ok(response)
}

fn offered_protocols(request: &Request) -> Vec<String> {
    request
        .headers()
        .get_all("Sec-WebSocket-Protocol")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
        .collect()
}

fn admit(server: &Server, request: &Request, offered: &[String]) -> Admission {
    let header = |name: &str| request.headers().get(name).and_then(|v| v.to_str().ok());
    if !header("Host").is_some_and(loopback_host) {
        tracing::warn!("refused an RPC upgrade with a non-loopback Host header");
        return Admission::Refused(CloseCode::NotAllowed);
    }
    if let Some(origin) = header("Origin")
        && !server.allowed_origins().iter().any(|o| o == origin)
    {
        tracing::warn!(origin, "refused an RPC upgrade from a browser origin");
        return Admission::Refused(CloseCode::NotAllowed);
    }
    let ticket = offered
        .iter()
        .find_map(|p| p.strip_prefix("prometheus-ticket."));
    let outcome = ticket.map_or(Redeemed::Unknown, |t| server.tickets.redeem(t));
    match outcome {
        Redeemed::Granted(grant) => Admission::Accepted(grant),
        refused => {
            // Never the ticket value: only what was wrong with it.
            tracing::warn!(outcome = ?refused, present = ticket.is_some(), "refused an RPC upgrade: bad ticket");
            Admission::Refused(CloseCode::BadCredential)
        }
    }
}

/// `127.0.0.1:<port>`, `localhost:<port>` or `[::1]:<port>`.
fn loopback_host(host: &str) -> bool {
    let name = match host.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    };
    let name = name.trim_start_matches('[').trim_end_matches(']');
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(pmt_net::listen::is_loopback)
}

fn close_message(code: CloseCode) -> Message {
    Message::Close(Some(CloseFrame {
        code: code.code().into(),
        reason: code.reason().into(),
    }))
}

/// Closes a refused connection without reading any frame from it.
async fn refuse(mut socket: Socket, code: CloseCode) {
    let _ = tokio::time::timeout(SEND_TIMEOUT, socket.send(close_message(code))).await;
    let _ = tokio::time::timeout(Duration::from_secs(1), socket.close(None)).await;
}

/// Serves a client protocol connection another listener admitted and
/// upgraded (the dashboard's `/api/rpc`, which authenticates with its own
/// session cookie instead of a ticket): from `rpc.hello` on, it is any
/// other connection.
/// `ends` closes it from outside.
pub async fn serve_upgraded<S: Transport>(
    server: Arc<Server>,
    socket: WebSocketStream<S>,
    grant: Grant,
    ends: Ends,
) {
    run(server, socket, grant, ends).await;
}

/// What ends a connection another listener admitted, besides the daemon
/// stopping.
#[derive(Debug, Clone, Default)]
pub struct Ends {
    /// The listener stops: closed with 1001 while the daemon keeps running.
    pub stop: CancellationToken,
    /// The credential it was admitted with is gone (a revoked device, a
    /// signed-out browser): closed with 4401.
    pub revoked: CancellationToken,
}

async fn run<S: Transport>(
    server: Arc<Server>,
    socket: WebSocketStream<S>,
    grant: Grant,
    ends: Ends,
) {
    let (sink, stream) = socket.split();
    let (tx, rx) = mpsc::unbounded_channel();
    let dead = CancellationToken::new();
    let (ordered, queue) = mpsc::unbounded_channel();
    let conn = Arc::new(Conn {
        id: server.next_conn_id(),
        grant,
        tx,
        hello: OnceLock::new(),
        ordered,
    });
    server.connections.fetch_add(1, Ordering::Relaxed);
    let writer = pmt_core::spawn::spawn(write_loop(sink, rx, dead.clone()).in_current_span());
    let in_order = pmt_core::spawn::spawn(ordered_loop(queue).in_current_span());
    read_loop(&server, &conn, stream, &dead, &ends).await;
    in_order.abort();
    server.hub().detach_all(conn.id);
    server.connections.fetch_sub(1, Ordering::Relaxed);
    drop(conn);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
}

async fn write_loop<S: Transport>(
    mut sink: SplitSink<WebSocketStream<S>, Message>,
    mut rx: mpsc::UnboundedReceiver<Message>,
    dead: CancellationToken,
) {
    while let Some(message) = rx.recv().await {
        let closing = matches!(message, Message::Close(_));
        let sent = tokio::time::timeout(SEND_TIMEOUT, sink.send(message)).await;
        if !matches!(sent, Ok(Ok(()))) || closing {
            break;
        }
    }
    dead.cancel();
}

async fn read_loop<S: Transport>(
    server: &Arc<Server>,
    conn: &Arc<Conn>,
    mut stream: SplitStream<WebSocketStream<S>>,
    dead: &CancellationToken,
    ends: &Ends,
) {
    loop {
        let next = tokio::select! {
            () = dead.cancelled() => return,
            () = server.closing.cancelled() => {
                let _ = conn.tx.send(close_message(CloseCode::ShuttingDown));
                return;
            }
            () = ends.stop.cancelled() => {
                let _ = conn.tx.send(close_message(CloseCode::ShuttingDown));
                return;
            }
            () = ends.revoked.cancelled() => {
                let _ = conn.tx.send(close_message(CloseCode::BadCredential));
                return;
            }
            next = tokio::time::timeout(IDLE_TIMEOUT, stream.next()) => next,
        };
        let flow = match next {
            Err(_) => close(conn, CloseCode::HeartbeatTimeout),
            Ok(None | Some(Ok(Message::Close(_)))) => ControlFlow::Break(()),
            Ok(Some(Ok(Message::Text(text)))) => text_frame(server, conn, text.as_str()),
            Ok(Some(Ok(_))) => ControlFlow::Continue(()),
            Ok(Some(Err(WsError::Capacity(_)))) => close(conn, CloseCode::FrameTooLarge),
            Ok(Some(Err(_))) => ControlFlow::Break(()),
        };
        if flow.is_break() {
            return;
        }
    }
}

fn close(conn: &Conn, code: CloseCode) -> ControlFlow<()> {
    let _ = conn.tx.send(close_message(code));
    ControlFlow::Break(())
}

fn send(conn: &Conn, text: String) {
    let _ = conn.tx.send(Message::text(text));
}

fn text_frame(server: &Arc<Server>, conn: &Arc<Conn>, text: &str) -> ControlFlow<()> {
    match frame::parse(text) {
        Err((id, err)) => send(conn, frame::error(id.as_ref(), &err)),
        Ok(Incoming::Request {
            id,
            method,
            params,
            traceparent,
        }) => {
            let traceparent = traceparent
                .as_deref()
                .and_then(pmt_obs::otel::TraceContext::parse);
            return request(server, conn, (id, traceparent), &method, params);
        }
        Ok(Incoming::Response {
            id: Id::Str(id),
            outcome,
        }) => {
            server
                .hub()
                .resolve(conn.id, &conn.client_id(), &id, outcome);
        }
        Ok(Incoming::Response { .. } | Incoming::Notification { .. }) => {}
    }
    ControlFlow::Continue(())
}

fn request(
    server: &Arc<Server>,
    conn: &Arc<Conn>,
    (id, traceparent): (Id, Option<pmt_obs::otel::TraceContext>),
    method: &str,
    params: Value,
) -> ControlFlow<()> {
    if conn.hello.get().is_none() {
        if method == "rpc.hello" {
            return hello(server, conn, &id, &params);
        }
        let err = Error::new(&codes::RPC_HANDSHAKE_REQUIRED, "handshake required");
        send(conn, frame::error(Some(&id), &RpcError::from_error(&err)));
        return ControlFlow::Continue(());
    }
    if method == "rpc.ping" {
        send(conn, frame::result(&id, &json!({"ok": true})));
        return ControlFlow::Continue(());
    }
    if server.retiring() && !WHILE_RETIRING.contains(&method) && method != "rpc.hello" {
        let err = Error::new(
            &codes::RPC_RETIRING,
            "daemon is restarting; reconnect to continue",
        );
        send(conn, frame::error(Some(&id), &RpcError::from_error(&err)));
        return ControlFlow::Continue(());
    }
    let call = Call {
        server: Arc::clone(server),
        conn: Arc::clone(conn),
    };
    let long = server.catalog().entry(method).is_some_and(|e| e.long);
    let job = Job {
        call,
        id,
        method: method.to_owned(),
        params,
        traceparent,
    };
    if long {
        let workers = Arc::clone(&server.workers);
        pmt_core::spawn::spawn(
            async move {
                let Ok(_permit) = workers.acquire_owned().await else {
                    return;
                };
                job.run().await;
            }
            .in_current_span(),
        );
    } else {
        let _ = conn.ordered.send(job);
    }
    ControlFlow::Continue(())
}

/// One request to answer.
#[derive(Debug)]
pub struct Job {
    call: Call,
    id: Id,
    method: String,
    params: Value,
    /// The caller's trace context (`meta.traceparent`).
    traceparent: Option<pmt_obs::otel::TraceContext>,
}

impl Job {
    /// Runs the handler in a task of its own, so a panic answers `-32099
    /// internal error: <method>` (with a crash report when the daemon
    /// keeps them) instead of ending the connection.
    async fn run(self) {
        let Self {
            call,
            id,
            method,
            params,
            traceparent,
        } = self;
        // The request's `rpc.request` span, under the caller's context.
        let mut span = pmt_obs::otel::Standalone::start(
            "rpc.request",
            pmt_obs::otel::context::derived(&format!("rpc:{}", random_hex(8))),
            traceparent,
        );
        span.attribute("rpc.method", &method);
        let conn = Arc::clone(&call.conn);
        let server = Arc::clone(&call.server);
        let minor = conn.hello.get().map_or(0, |h| h.minor);
        let name = method.clone();
        let reporter = server.crash_reporter().cloned();
        let catalog = Arc::clone(&server);
        let handler = pmt_core::spawn::spawn(
            async move {
                let work = server.router().dispatch(call, &name, params);
                match reporter {
                    Some(reporter) => {
                        let task = format!("rpc-{name}-{}", random_hex(4));
                        pmt_core::panic::isolate(&task, &reporter, work).await.ok()
                    }
                    None => Some(work.await),
                }
            }
            .in_current_span(),
        );
        let outcome = if let Ok(Some(outcome)) = handler.await {
            outcome
        } else {
            tracing::error!(method, "an RPC handler panicked");
            let err = Error::new(&codes::RPC_INTERNAL, format!("internal error: {method}"));
            Err(RpcError::from_error(&err))
        };
        let (reply, error) = match outcome {
            Ok(mut result) => {
                catalog
                    .catalog()
                    .strip_newer(&method, &mut result, true, minor);
                (frame::result(&id, &result), None)
            }
            Err(err) => (frame::error(Some(&id), &err), Some(err.code.to_string())),
        };
        span.end(error);
        send(&conn, reply);
    }
}

/// Answers the connection's ordinary requests one at a time, in arrival
/// order.
async fn ordered_loop(mut queue: mpsc::UnboundedReceiver<Job>) {
    while let Some(job) = queue.recv().await {
        job.run().await;
    }
}

fn hello(server: &Server, conn: &Conn, id: &Id, params: &Value) -> ControlFlow<()> {
    if let Err(err) = server.catalog().check_params("rpc.hello", params) {
        send(conn, frame::error(Some(id), &err));
        return ControlFlow::Continue(());
    }
    let Ok(hello) = serde_json::from_value::<HelloParams>(params.clone()) else {
        let err = RpcError::invalid_params("", false, "invalid rpc.hello params");
        send(conn, frame::error(Some(id), &err));
        return ControlFlow::Continue(());
    };
    let daemon = server.protocol();
    if hello.protocol.major != daemon.major {
        let err = Error::new(
            &codes::RPC_PROTOCOL_MISMATCH,
            format!(
                "protocol mismatch: client {}, daemon {daemon}; run 'prometheus update'",
                hello.protocol
            ),
        );
        send(conn, frame::error(Some(id), &RpcError::from_error(&err)));
        return close(conn, CloseCode::ProtocolUnsupported);
    }
    let minor = hello.protocol.minor;
    let advertised = hello.capabilities.events;
    let mut result =
        serde_json::to_value(server.hello_result(&conn.grant.profiles, minor, &advertised))
            .unwrap_or_default();
    if let Err(violation) = server.catalog().check_outbound("rpc.hello", &result, true) {
        server.violations().report("rpc.hello", &violation);
    }
    server
        .catalog()
        .strip_newer("rpc.hello", &mut result, true, minor);
    let _ = conn.hello.set(HelloState {
        client: hello.client,
        minor,
        advertised: Arc::new(advertised),
        server_requests: hello.capabilities.server_requests,
        features: Arc::new(hello.capabilities.features.clone()),
    });
    send(conn, frame::result(id, &result));
    ControlFlow::Continue(())
}

#[cfg(test)]
mod tests {
    use super::loopback_host;

    #[test]
    fn only_loopback_hosts_are_admitted() {
        for host in [
            "127.0.0.1:9",
            "localhost:9",
            "LOCALHOST",
            "[::1]:9",
            "127.0.0.1",
        ] {
            assert!(loopback_host(host), "{host}");
        }
        for host in [
            "evil.example:9",
            "10.0.0.1:9",
            "localhost.evil.example",
            "[::]:9",
        ] {
            assert!(!loopback_host(host), "{host}");
        }
    }
}
