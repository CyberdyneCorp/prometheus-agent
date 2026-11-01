//! A minimal Chrome DevTools Protocol client over one WebSocket.
//!
//! Commands are JSON `{id, method, params, sessionId?}`; replies carry the
//! same `id`. Messages without an `id` are events, fanned out to every
//! subscriber. Page-level commands go to a flattened target session
//! (`Target.attachToTarget` with `flatten: true`), named by `sessionId`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

/// One CDP event.
#[derive(Debug, Clone)]
pub struct Event {
    /// `Domain.event`.
    pub method: String,
    /// Its parameters.
    pub params: Value,
    /// The target session it came from (`None`: the browser).
    pub session_id: Option<String>,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// A connection to a browser's DevTools endpoint.
pub struct Cdp {
    endpoint: String,
    next_id: AtomicU64,
    outgoing: mpsc::UnboundedSender<String>,
    pending: Pending,
    events: broadcast::Sender<Event>,
    closed: Arc<tokio::sync::Notify>,
    alive: Arc<std::sync::atomic::AtomicBool>,
}

impl std::fmt::Debug for Cdp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cdp")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

/// Default bound on one command.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

impl Cdp {
    /// Connects to the `ws://` DevTools endpoint.
    ///
    /// # Errors
    ///
    /// The connection or handshake failed.
    pub async fn connect(endpoint: &str) -> Result<Arc<Self>, String> {
        let (socket, _) = tokio_tungstenite::connect_async(endpoint)
            .await
            .map_err(|err| format!("cannot connect to {endpoint}: {err}"))?;
        let (mut sink, mut stream) = socket.split();
        let (outgoing, mut queue) = mpsc::unbounded_channel::<String>();
        let pending: Pending = Arc::default();
        let (events, _) = broadcast::channel(256);
        let closed = Arc::new(tokio::sync::Notify::new());
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        tokio::spawn(async move {
            while let Some(text) = queue.recv().await {
                if sink.send(Message::text(text)).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        let reader = Reader {
            pending: Arc::clone(&pending),
            events: events.clone(),
        };
        let (reader_closed, reader_alive) = (Arc::clone(&closed), Arc::clone(&alive));
        tokio::spawn(async move {
            while let Some(Ok(message)) = stream.next().await {
                if let Message::Text(text) = message {
                    reader.dispatch(text.as_str());
                }
            }
            reader_alive.store(false, Ordering::SeqCst);
            reader.fail_all();
            reader_closed.notify_waiters();
        });
        Ok(Arc::new(Self {
            endpoint: endpoint.to_owned(),
            next_id: AtomicU64::new(1),
            outgoing,
            pending,
            events,
            closed,
            alive,
        }))
    }

    /// The endpoint this connection is bound to.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Whether the WebSocket is still open.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Resolves when the WebSocket closes.
    pub async fn closed(&self) {
        let notified = self.closed.notified();
        if self.is_alive() {
            notified.await;
        }
    }

    /// Events from now on.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Sends `method` (to the target `session`, if any) and waits up to
    /// [`COMMAND_TIMEOUT`] for its result.
    ///
    /// # Errors
    ///
    /// The protocol error message, a closed connection or a timeout.
    pub async fn call(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, String> {
        self.call_within(method, params, session, COMMAND_TIMEOUT)
            .await
    }

    /// [`Cdp::call`] with an explicit bound.
    ///
    /// # Errors
    ///
    /// As [`Cdp::call`].
    pub async fn call_within(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
        limit: Duration,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        let (reply, answer) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, reply);
        if self.outgoing.send(message.to_string()).is_err() {
            return Err("the browser connection is closed".to_owned());
        }
        match tokio::time::timeout(limit, answer).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("the browser connection is closed".to_owned()),
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
                Err(format!("{method} timed out after {} s", limit.as_secs()))
            }
        }
    }
}

struct Reader {
    pending: Pending,
    events: broadcast::Sender<Event>,
}

impl Reader {
    fn dispatch(&self, text: &str) {
        let Ok(message) = serde_json::from_str::<Value>(text) else {
            return;
        };
        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            let reply = self
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            if let Some(reply) = reply {
                let result = match message.get("error") {
                    Some(error) => Err(error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("protocol error")
                        .to_owned()),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = reply.send(result);
            }
            return;
        }
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            let _ = self.events.send(Event {
                method: method.to_owned(),
                params: message.get("params").cloned().unwrap_or(Value::Null),
                session_id: message
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        }
    }

    fn fail_all(&self) {
        let waiting: Vec<_> = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
            .collect();
        for (_, reply) in waiting {
            let _ = reply.send(Err("the browser connection closed".to_owned()));
        }
    }
}
