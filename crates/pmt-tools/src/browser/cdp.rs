//! A minimal Chrome DevTools Protocol client over one WebSocket, or over
//! the pipe pair of a browser started with `--remote-debugging-pipe`.
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

/// Sends each NUL-ended frame read from `pipe` until it ends or nobody
/// listens.
#[cfg(windows)]
fn read_frames(pipe: std::io::PipeReader, frames: &mpsc::UnboundedSender<String>) {
    use std::io::BufRead as _;

    let mut reader = std::io::BufReader::new(pipe);
    loop {
        let mut frame = Vec::new();
        match reader.read_until(0, &mut frame) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if frame.last() == Some(&0) {
            frame.pop();
        }
        if frames
            .send(String::from_utf8_lossy(&frame).into_owned())
            .is_err()
        {
            return;
        }
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
        let (mut sink, stream) = socket.split();
        let incoming = stream
            .take_while(|message| std::future::ready(message.is_ok()))
            .filter_map(|message| {
                std::future::ready(match message {
                    Ok(Message::Text(text)) => Some(text.as_str().to_owned()),
                    _ => None,
                })
            });
        let writer = |mut queue: mpsc::UnboundedReceiver<String>| async move {
            while let Some(text) = queue.recv().await {
                if sink.send(Message::text(text)).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        };
        Ok(Self::assemble(endpoint, writer, incoming))
    }

    /// A connection over Chromium's `--remote-debugging-pipe`: messages
    /// are JSON texts each ended by a NUL byte, written to the browser on
    /// `to_browser` (its fd 3) and read from `from_browser` (its fd 4).
    /// `label` stands in for the endpoint (a pipe cannot be reconnected).
    ///
    /// # Errors
    ///
    /// A descriptor cannot be registered with the runtime.
    #[cfg(unix)]
    pub fn over_pipe(
        label: &str,
        to_browser: std::os::fd::OwnedFd,
        from_browser: std::os::fd::OwnedFd,
    ) -> Result<Arc<Self>, String> {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
        use tokio::net::unix::pipe;

        let register = |err: std::io::Error| format!("cannot use the browser pipe: {err}");
        let mut sender = pipe::Sender::from_owned_fd(to_browser).map_err(register)?;
        let receiver = pipe::Receiver::from_owned_fd(from_browser).map_err(register)?;
        let incoming = futures_util::stream::unfold(
            tokio::io::BufReader::new(receiver),
            |mut reader| async move {
                let mut frame = Vec::new();
                match reader.read_until(0, &mut frame).await {
                    Ok(0) | Err(_) => None,
                    Ok(_) => {
                        if frame.last() == Some(&0) {
                            frame.pop();
                        }
                        Some((String::from_utf8_lossy(&frame).into_owned(), reader))
                    }
                }
            },
        );
        let writer = |mut queue: mpsc::UnboundedReceiver<String>| async move {
            while let Some(text) = queue.recv().await {
                let mut frame = text.into_bytes();
                frame.push(0);
                if sender.write_all(&frame).await.is_err() {
                    break;
                }
            }
        };
        Ok(Self::assemble(label, writer, incoming))
    }

    /// [`Cdp::over_pipe`] on Windows: the browser reads `to_browser` and
    /// writes `from_browser`, anonymous pipes it inherited and found
    /// through `--remote-debugging-io-pipes`. Anonymous pipes have no
    /// overlapped I/O, so each direction runs on a thread of its own; the
    /// reader ends with the pipe (the browser and its launcher exited).
    ///
    /// # Errors
    ///
    /// The reader thread cannot be started.
    #[cfg(windows)]
    pub fn over_pipe(
        label: &str,
        to_browser: std::io::PipeWriter,
        from_browser: std::io::PipeReader,
    ) -> Result<Arc<Self>, String> {
        use std::io::Write as _;

        let (frames, received) = mpsc::unbounded_channel::<String>();
        std::thread::Builder::new()
            .name("cdp-pipe-reader".to_owned())
            .spawn(move || read_frames(from_browser, &frames))
            .map_err(|err| format!("cannot read the browser pipe: {err}"))?;
        let incoming = futures_util::stream::unfold(received, |mut received| async move {
            received.recv().await.map(|frame| (frame, received))
        });
        let writer = |mut queue: mpsc::UnboundedReceiver<String>| async move {
            let mut pipe = to_browser;
            while let Some(text) = queue.recv().await {
                let mut frame = text.into_bytes();
                frame.push(0);
                let written =
                    tokio::task::spawn_blocking(move || pipe.write_all(&frame).map(|()| pipe))
                        .await;
                match written {
                    Ok(Ok(back)) => pipe = back,
                    _ => break,
                }
            }
        };
        Ok(Self::assemble(label, writer, incoming))
    }

    /// Starts the writer (draining the outgoing queue) and the reader
    /// (dispatching replies and events until the stream ends).
    fn assemble<W, F>(
        endpoint: &str,
        writer: W,
        incoming: impl futures_util::Stream<Item = String> + Send + 'static,
    ) -> Arc<Self>
    where
        W: FnOnce(mpsc::UnboundedReceiver<String>) -> F,
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let (outgoing, queue) = mpsc::unbounded_channel::<String>();
        let pending: Pending = Arc::default();
        let (events, _) = broadcast::channel(256);
        let closed = Arc::new(tokio::sync::Notify::new());
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        tokio::spawn(writer(queue));
        let reader = Reader {
            pending: Arc::clone(&pending),
            events: events.clone(),
        };
        let (reader_closed, reader_alive) = (Arc::clone(&closed), Arc::clone(&alive));
        tokio::spawn(async move {
            let mut incoming = std::pin::pin!(incoming);
            while let Some(text) = incoming.next().await {
                reader.dispatch(&text);
            }
            reader_alive.store(false, Ordering::SeqCst);
            reader.fail_all();
            reader_closed.notify_waiters();
        });
        Arc::new(Self {
            endpoint: endpoint.to_owned(),
            next_id: AtomicU64::new(1),
            outgoing,
            pending,
            events,
            closed,
            alive,
        })
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
        // The reader fails every pending call when the connection ends; a
        // call registered after that would wait out its whole bound, so a
        // closed connection is checked once the call is registered (the
        // reader clears `alive` before it drains the pending calls).
        if !self.is_alive() || self.outgoing.send(message.to_string()).is_err() {
            self.pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
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

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    /// A DevTools endpoint that accepts one connection and closes it.
    async fn closing_endpoint() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = socket.close(None).await;
        });
        format!("ws://{address}/devtools/browser/test")
    }

    #[tokio::test]
    async fn a_call_after_the_connection_closed_fails_at_once() {
        let cdp = Cdp::connect(&closing_endpoint().await).await.unwrap();
        cdp.closed().await;
        let started = Instant::now();
        let result = cdp
            .call_within(
                "Target.getTargets",
                json!({}),
                None,
                Duration::from_secs(20),
            )
            .await;
        assert_eq!(result, Err("the browser connection is closed".to_owned()));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(cdp.pending.lock().unwrap().is_empty());
    }
}
