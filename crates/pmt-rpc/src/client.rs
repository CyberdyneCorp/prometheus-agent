//! The client side of the protocol: a ticket from the control channel, the
//! WebSocket upgrade carrying it as a subprotocol, `rpc.hello`, then calls,
//! events and server requests over one connection, with the 15 s heartbeat
//! (spec: workspace-architecture, Ticket-authenticated loopback attachment;
//! Protocol handshake and versioning; Connection liveness and backpressure).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use pmt_core::error::{Error, codes};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::catalog::contracts::{Envelope, HelloParams, HelloResult, Method};
use crate::codes::{CloseCode, for_kind};
use crate::control::{self, ControlRequest, Purpose};
use crate::discovery::Endpoint;
use crate::frame::{self, Id, Incoming, RpcError};

/// The raw WebSocket of a client connection.
pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How often a client pings.
pub const HEARTBEAT: Duration = Duration::from_secs(15);
/// Silence after which a client treats the connection as dead.
pub const DEAD_AFTER: Duration = Duration::from_secs(45);

/// The subprotocols of an upgrade: the ticket and the protocol major.
#[must_use]
pub fn subprotocols(ticket: &str, major: u16) -> String {
    format!("prometheus-rpc.v{major}, prometheus-ticket.{ticket}")
}

/// Opens the WebSocket at `rpc_url` with `subprotocols` (no handshake).
///
/// # Errors
///
/// `rpc.connection` when the upgrade fails.
pub async fn dial(rpc_url: &str, subprotocols: &str) -> Result<Socket, Error> {
    let failed = |e: &dyn std::fmt::Display| {
        Error::new(
            &codes::RPC_CONNECTION,
            format!("cannot connect to the daemon at {rpc_url}: {e}"),
        )
    };
    let mut request = rpc_url.into_client_request().map_err(|e| failed(&e))?;
    let value = HeaderValue::from_str(subprotocols).map_err(|e| failed(&e))?;
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", value);
    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| failed(&e))?;
    Ok(socket)
}

/// A daemon-to-client request waiting for [`Client::respond`].
#[derive(Debug, Clone, PartialEq)]
pub struct ServerCall {
    /// `srq-<12 hex>`.
    pub id: String,
    /// The request name.
    pub method: String,
    /// Its params.
    pub params: Value,
}

/// What the daemon sends without being asked.
#[derive(Debug)]
pub struct Inbox {
    /// Events, in arrival order.
    pub events: mpsc::UnboundedReceiver<Envelope>,
    /// Server-to-client requests.
    pub requests: mpsc::UnboundedReceiver<ServerCall>,
}

/// How a connection ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Closed {
    /// The close code the daemon sent, when it sent one.
    pub code: Option<u16>,
    /// Its reason.
    pub reason: String,
}

type Pending = Arc<Mutex<HashMap<Id, oneshot::Sender<Result<Value, RpcError>>>>>;

/// An attached connection.
#[derive(Debug, Clone)]
pub struct Client {
    outbound: mpsc::UnboundedSender<Message>,
    pending: Pending,
    next_id: Arc<AtomicI64>,
    closed: watch::Receiver<Option<Closed>>,
    hello: Arc<HelloResult>,
}

impl Client {
    /// Gets a ticket for `profiles` over the endpoint's control channel,
    /// dials and performs `rpc.hello`. The heartbeat starts at once.
    ///
    /// # Errors
    ///
    /// `rpc.ticket`, `rpc.connection`, or the `rpc.hello` error (a
    /// protocol mismatch is `rpc.protocol_mismatch`, exit 76).
    pub async fn connect(
        endpoint: &Endpoint,
        purpose: Purpose,
        profiles: Vec<String>,
        hello: &HelloParams,
    ) -> Result<(Self, Inbox), Error> {
        let request = ControlRequest::Ticket {
            purpose,
            profiles,
            pid: Some(std::process::id()),
        };
        let ticket = control::ticket_of(control::call(&endpoint.control, &request).await?)?;
        let socket = dial(
            &endpoint.rpc_url,
            &subprotocols(&ticket.ticket, hello.protocol.major),
        )
        .await?;
        Self::over(socket, hello).await
    }

    /// Performs `rpc.hello` over an open socket.
    ///
    /// # Errors
    ///
    /// As [`Client::connect`].
    pub async fn over(socket: Socket, hello: &HelloParams) -> Result<(Self, Inbox), Error> {
        let (client, inbox) = Self::spawn(socket);
        let params = serde_json::to_value(hello).unwrap_or_default();
        let result = client
            .call("rpc.hello", params)
            .await
            .map_err(|e| error_of(&e))?;
        let hello: HelloResult = serde_json::from_value(result).map_err(|e| {
            Error::new(
                &codes::RPC_CONNECTION,
                format!("malformed rpc.hello result: {e}"),
            )
        })?;
        let client = Self {
            hello: Arc::new(hello),
            ..client
        };
        tokio::spawn(heartbeat(client.clone()));
        Ok((client, inbox))
    }

    fn spawn(socket: Socket) -> (Self, Inbox) {
        let (sink, stream) = socket.split();
        let (outbound, rx) = mpsc::unbounded_channel();
        let (closed_tx, closed) = watch::channel(None);
        let (events_tx, events) = mpsc::unbounded_channel();
        let (requests_tx, requests) = mpsc::unbounded_channel();
        let pending: Pending = Arc::default();
        tokio::spawn(write_loop(sink, rx));
        tokio::spawn(read_loop(
            stream,
            Arc::clone(&pending),
            Routes {
                events: events_tx,
                requests: requests_tx,
                closed: closed_tx,
            },
        ));
        let placeholder = HelloResult {
            server_version: String::new(),
            build_id: String::new(),
            instance_id: String::new(),
            epoch: String::new(),
            protocol: crate::catalog::contracts::Version { major: 0, minor: 0 },
            profiles: Vec::new(),
            methods_digest: String::new(),
            capabilities: crate::catalog::contracts::ServerCapabilities::default(),
            platform_support: crate::catalog::contracts::PlatformSupport::default(),
        };
        let client = Self {
            outbound,
            pending,
            next_id: Arc::new(AtomicI64::new(1)),
            closed,
            hello: Arc::new(placeholder),
        };
        (client, Inbox { events, requests })
    }

    /// The daemon's `rpc.hello` answer.
    #[must_use]
    pub fn hello(&self) -> &HelloResult {
        &self.hello
    }

    /// Calls `method` and waits for its result.
    ///
    /// # Errors
    ///
    /// The daemon's error, or `-32006 rpc.connection` when the connection
    /// closed first.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = Id::Num(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (reply, answer) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), reply);
        let text = frame::request(&id, method, &params);
        if self.outbound.send(Message::text(text)).is_err() {
            return Err(self.closed_error());
        }
        let mut closed = self.closed.clone();
        tokio::select! {
            answer = answer => answer.unwrap_or_else(|_| Err(self.closed_error())),
            _ = closed.wait_for(Option::is_some) => Err(self.closed_error()),
        }
    }

    /// Calls the typed method `M`.
    ///
    /// # Errors
    ///
    /// As [`Client::call`]; a result that does not match `M`'s contract is
    /// `-32099`.
    pub async fn call_typed<M: Method>(&self, params: &M::Params) -> Result<M::Result, RpcError> {
        let params = serde_json::to_value(params).unwrap_or_default();
        let value = self.call(M::NAME, params).await?;
        serde_json::from_value(value).map_err(|e| {
            RpcError::from_error(&Error::new(
                &codes::RPC_CONNECTION,
                format!("malformed {} result: {e}", M::NAME),
            ))
        })
    }

    /// Sends a raw text frame (debug clients and tests).
    pub fn send_raw(&self, text: String) {
        let _ = self.outbound.send(Message::text(text));
    }

    /// Answers a server request.
    pub fn respond(&self, id: &str, result: Result<Value, RpcError>) {
        let id = Id::Str(id.to_owned());
        let text = match result {
            Ok(value) => frame::result(&id, &value),
            Err(err) => frame::error(Some(&id), &err),
        };
        self.send_raw(text);
    }

    /// Closes the connection normally.
    pub fn close(&self) {
        let _ = self
            .outbound
            .send(Message::Close(Some(close_frame(CloseCode::Normal))));
    }

    /// How the connection ended, once it has.
    #[must_use]
    pub fn closed(&self) -> Option<Closed> {
        self.closed.borrow().clone()
    }

    /// Waits until the connection ends.
    pub async fn wait_closed(&self) -> Closed {
        let mut closed = self.closed.clone();
        let ended = closed.wait_for(Option::is_some).await;
        ended.ok().and_then(|c| c.clone()).unwrap_or(Closed {
            code: None,
            reason: "closed".into(),
        })
    }

    fn closed_error(&self) -> RpcError {
        let closed = self.closed();
        let detail = match closed {
            Some(Closed {
                code: Some(code),
                reason,
            }) => format!("closed with {code} ({reason})"),
            _ => "closed".to_owned(),
        };
        let err = Error::new(
            &codes::RPC_CONNECTION,
            format!("connection to the daemon {detail}"),
        );
        RpcError::from_error(&err)
    }
}

/// The notice a client shows when the daemon runs another build (spec:
/// workspace-architecture, Stale daemon build detection); `None` when the
/// builds match.
#[must_use]
pub fn stale_build_notice(hello: &HelloResult, own_build_id: &str) -> Option<String> {
    (hello.build_id != own_build_id).then(|| {
        format!(
            "daemon is running an older build ({}); restart with 'prometheus daemon restart'",
            hello.server_version
        )
    })
}

/// An [`RpcError`] as a CLI error (kind from `data`, protocol mismatch
/// keeps its code so the CLI exits 76).
#[must_use]
pub fn error_of(err: &RpcError) -> Error {
    let catalog = err
        .data
        .as_ref()
        .and_then(|d| {
            pmt_core::error::ERROR_CODES
                .iter()
                .find(|c| c.code == d.code)
        })
        .copied()
        .or_else(|| {
            pmt_core::error::ERROR_CODES
                .iter()
                .find(|c| for_kind(c.kind) == err.code)
                .copied()
        })
        .unwrap_or(&codes::RPC_CONNECTION);
    let mut out = Error::new(catalog, err.message.clone());
    if let Some(hint) = err.data.as_ref().and_then(|d| d.hint.clone()) {
        out = out.with_hint(hint);
    }
    out
}

fn close_frame(code: CloseCode) -> tokio_tungstenite::tungstenite::protocol::CloseFrame {
    tokio_tungstenite::tungstenite::protocol::CloseFrame {
        code: code.code().into(),
        reason: code.reason().into(),
    }
}

async fn heartbeat(client: Client) {
    let mut tick = tokio::time::interval(HEARTBEAT);
    tick.tick().await;
    loop {
        tick.tick().await;
        if client.closed().is_some() {
            return;
        }
        let ping = tokio::time::timeout(DEAD_AFTER, client.call("rpc.ping", serde_json::json!({})));
        if !matches!(ping.await, Ok(Ok(_))) {
            let _ = client.outbound.send(Message::Close(Some(close_frame(
                CloseCode::HeartbeatTimeout,
            ))));
            return;
        }
    }
}

async fn write_loop(
    mut sink: futures_util::stream::SplitSink<Socket, Message>,
    mut rx: mpsc::UnboundedReceiver<Message>,
) {
    while let Some(message) = rx.recv().await {
        let closing = matches!(message, Message::Close(_));
        if sink.send(message).await.is_err() || closing {
            break;
        }
    }
}

struct Routes {
    events: mpsc::UnboundedSender<Envelope>,
    requests: mpsc::UnboundedSender<ServerCall>,
    closed: watch::Sender<Option<Closed>>,
}

async fn read_loop(
    mut stream: futures_util::stream::SplitStream<Socket>,
    pending: Pending,
    routes: Routes,
) {
    let mut ended = Closed {
        code: None,
        reason: "connection lost".into(),
    };
    while let Some(message) = stream.next().await {
        match message {
            Ok(Message::Text(text)) => route(text.as_str(), &pending, &routes).await,
            Ok(Message::Close(frame)) => {
                ended = Closed {
                    code: frame.as_ref().map(|f| u16::from(f.code)),
                    reason: frame.map(|f| f.reason.to_string()).unwrap_or_default(),
                };
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    pending.lock().await.clear();
    let _ = routes.closed.send(Some(ended));
}

async fn route(text: &str, pending: &Pending, routes: &Routes) {
    match frame::parse(text) {
        Ok(Incoming::Response { id, outcome }) => {
            if let Some(reply) = pending.lock().await.remove(&id) {
                let _ = reply.send(outcome);
            }
        }
        Ok(Incoming::Notification { params, .. }) => {
            if let Ok(envelope) = serde_json::from_value(params) {
                let _ = routes.events.send(envelope);
            }
        }
        Ok(Incoming::Request { id, method, params }) => {
            let _ = routes.requests.send(ServerCall {
                id: id.to_string(),
                method,
                params,
            });
        }
        // An error reply with `id: null` answers a frame we could not
        // send well-formed; nothing waits for it.
        Err(_) => {}
    }
}
