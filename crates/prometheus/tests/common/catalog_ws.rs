//! A mock platform serving a WebSocket and a REST API on one loopback
//! port, for catalog adapters whose events arrive over a socket the
//! bridge sidecar holds (Mattermost, `DingTalk`, `SimpleX`, Nostr relays).
//! Each accepted socket gets an outbox; frames the adapter sends go to
//! the `on_frame` callback, which may answer on that socket; REST
//! requests go to a [`Handler`] like [`MockHttp`]'s.

#![allow(dead_code)]

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::catalog::{Handler, Reply, Request};

/// One accepted socket, as the mock answers on it.
#[derive(Clone)]
pub struct Socket {
    tx: mpsc::UnboundedSender<String>,
    /// The upgrade request's path and query.
    pub path: String,
}

impl Socket {
    /// Sends a text frame.
    pub fn send(&self, frame: impl Into<String>) {
        let _ = self.tx.send(frame.into());
    }
}

/// Called with each text frame the adapter sends on a socket.
pub type OnFrame = Arc<dyn Fn(&Socket, &str) + Send + Sync>;
/// Called once per accepted socket (with its upgrade path); `false`
/// refuses it (closed at once).
pub type OnOpen = Arc<dyn Fn(&Socket) -> bool + Send + Sync>;

/// The mock server.
pub struct WsServer {
    /// Its port.
    pub port: u16,
    live: Arc<Mutex<Vec<Socket>>>,
}

impl WsServer {
    /// Starts it on the current Tokio runtime.
    pub async fn start(on_open: OnOpen, on_frame: OnFrame, rest: Handler) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let live: Arc<Mutex<Vec<Socket>>> = Arc::default();
        let sockets = Arc::clone(&live);
        pmt_core::spawn::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (on_open, on_frame, rest, live) = (
                    Arc::clone(&on_open),
                    Arc::clone(&on_frame),
                    Arc::clone(&rest),
                    Arc::clone(&sockets),
                );
                pmt_core::spawn::spawn(async move {
                    let mut peek = [0u8; 4096];
                    let n = stream.peek(&mut peek).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&peek[..n]).to_ascii_lowercase();
                    if head.contains("upgrade: websocket") {
                        let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                        websocket(stream, path, &on_open, &on_frame, &live).await;
                    } else {
                        Box::pin(http(stream, &rest)).await;
                    }
                });
            }
        });
        Self { port, live }
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// `ws://127.0.0.1:<port>`.
    pub fn ws(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }

    /// `127.0.0.1:<port>`, as a network grant.
    pub fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// Sends `frame` on every live socket; whether there was one.
    pub fn broadcast(&self, frame: &str) -> bool {
        let mut live = self.live.lock().unwrap();
        live.retain(|s| !s.tx.is_closed());
        for socket in live.iter() {
            socket.send(frame);
        }
        !live.is_empty()
    }

    /// How many sockets are open.
    pub fn open_sockets(&self) -> usize {
        let mut live = self.live.lock().unwrap();
        live.retain(|s| !s.tx.is_closed());
        live.len()
    }

    /// Closes every socket.
    pub fn drop_all(&self) {
        self.live.lock().unwrap().clear();
    }
}

async fn websocket(
    stream: TcpStream,
    path: String,
    on_open: &OnOpen,
    on_frame: &OnFrame,
    live: &Mutex<Vec<Socket>>,
) {
    let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let handle = Socket { tx, path };
    if !on_open(&handle) {
        let _ = socket.close(None).await;
        return;
    }
    live.lock().unwrap().push(handle.clone());
    // Dropping the handle's last sender (drop_all) ends this loop.
    let weak = handle.tx.downgrade();
    drop(handle);
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(frame) => { let _ = socket.send(Message::text(frame)).await; }
                None => break,
            },
            incoming = socket.next() => {
                let Some(Ok(message)) = incoming else { break };
                let Message::Text(text) = message else { continue };
                let Some(tx) = weak.upgrade() else { break };
                let path = String::new();
                on_frame(&Socket { tx, path }, text.as_str());
            }
        }
    }
    let _ = socket.close(None).await;
}

async fn http(mut stream: TcpStream, rest: &Handler) {
    let mut raw = Vec::new();
    let mut buf = vec![0u8; 16384];
    let (head, body) = loop {
        let Ok(n) = stream.read(&mut buf).await else {
            return;
        };
        if n == 0 {
            return;
        }
        raw.extend_from_slice(&buf[..n]);
        let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&raw[..end]).into_owned();
        let length: usize = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap_or(0))
            })
            .unwrap_or(0);
        if raw.len() >= end + 4 + length {
            break (head, raw[end + 4..end + 4 + length].to_vec());
        }
    };
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or_default().split_whitespace();
    let request = Request {
        method: first.next().unwrap_or_default().to_owned(),
        path: first.next().unwrap_or_default().to_owned(),
        headers: lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
            .collect(),
        body,
    };
    let rest = Arc::clone(rest);
    let reply: Reply = pmt_core::spawn::blocking(move || rest(&request))
        .await
        .unwrap_or_else(|_| Reply::status(500));
    let mut out = format!(
        "HTTP/1.1 {} X\r\ncontent-length: {}\r\nconnection: close\r\n",
        reply.status,
        reply.body.len()
    );
    for (k, v) in &reply.headers {
        let _ = write!(out, "{k}: {v}\r\n");
    }
    out.push_str("\r\n");
    let _ = stream.write_all(out.as_bytes()).await;
    let _ = stream.write_all(&reply.body).await;
}
