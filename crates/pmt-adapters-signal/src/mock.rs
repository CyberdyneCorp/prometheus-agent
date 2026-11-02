//! A signal-cli HTTP daemon mock: the health check, a server-sent event
//! stream that holds envelopes while no client listens (the daemon's
//! `on-connection` receive mode, where Signal keeps them), and JSON-RPC
//! `send` (8,000 characters at most), edits, reactions, typing, groups,
//! attachments and scripted failures. It speaks HTTP/1.1 by hand because
//! the event stream stays open.

// A fixture: a mock that cannot serve should fail loudly.
#![allow(clippy::missing_panics_doc)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use pmt_gateway::messaging::conformance::{Failure, MockPlatform, MockSent};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// The account the adapter runs as.
pub const ACCOUNT: &str = "+15550000001";
/// The person the kit talks as (and their DM).
pub const USER: &str = "+15550004242";
const LIMIT: usize = 8000;

#[derive(Default)]
struct State {
    pending: Vec<Value>,
    last: Option<Value>,
    next: i64,
    sent: Vec<MockSent>,
    typing: usize,
    reactions: Vec<(String, String)>,
    failures: VecDeque<Failure>,
    attachments: Vec<(String, Vec<u8>)>,
    uploads: Vec<String>,
    listener: Option<mpsc::UnboundedSender<Option<Value>>>,
}

/// The mock.
pub struct SignalMock {
    state: Mutex<State>,
    addr: SocketAddr,
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

async fn read_request(reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Option<Request> {
    let mut line = String::new();
    if reader.read_line(&mut line).await.ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next()?.to_owned(), parts.next()?.to_owned());
    let mut length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).await.ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await.ok()?;
    Some(Request { method, path, body })
}

fn response(status: u16, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
}

impl SignalMock {
    /// Starts the daemon on a free loopback port.
    pub async fn start() -> Arc<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock = Arc::new(Self {
            state: Mutex::new(State {
                next: 1_700_000_000_000,
                ..State::default()
            }),
            addr: listener.local_addr().unwrap(),
        });
        let server = Arc::clone(&mock);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(Arc::clone(&server).connection(stream));
            }
        });
        mock
    }

    /// The table lines that point the adapter at this mock.
    pub fn table(&self) -> String {
        format!(
            "options = {{ http_url = \"http://{}\", account = \"{ACCOUNT}\" }}",
            self.addr
        )
    }

    /// Stores an attachment the daemon serves by id.
    pub fn add_attachment(&self, id: &str, bytes: Vec<u8>) {
        self.lock().attachments.push((id.to_owned(), bytes));
    }

    /// Paths of the attachments sent.
    pub fn uploads(&self) -> Vec<String> {
        self.lock().uploads.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn ts(&self) -> i64 {
        let mut state = self.lock();
        state.next += 1;
        state.next
    }

    /// Delivers a data message from `USER` with the given fields added;
    /// its message id.
    pub fn receive(&self, data_message: &Value) -> String {
        let ts = self.ts();
        let mut message = data_message.clone();
        message["timestamp"] = json!(ts);
        let event = json!({"envelope": {"source": USER, "sourceNumber": USER, "sourceName": "Kit",
                                        "timestamp": ts, "dataMessage": message},
                           "account": ACCOUNT});
        self.lock().last = Some(event.clone());
        self.publish(event);
        format!("{USER}:{ts}")
    }

    fn publish(&self, event: Value) {
        let mut state = self.lock();
        let delivered = state
            .listener
            .as_ref()
            .is_some_and(|l| l.send(Some(event.clone())).is_ok());
        if !delivered {
            state.pending.push(event);
        }
    }

    async fn connection(self: Arc<Self>, stream: TcpStream) {
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        while let Some(request) = read_request(&mut reader).await {
            let answer = match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/api/v1/check") => response(200, ""),
                ("GET", "/api/v1/events") => return self.events(writer).await,
                ("POST", "/api/v1/rpc") => self.rpc(&request.body),
                _ => response(404, ""),
            };
            if writer.write_all(answer.as_bytes()).await.is_err() {
                return;
            }
        }
    }

    async fn events(&self, mut writer: tokio::net::tcp::OwnedWriteHalf) {
        let head =
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
        if writer.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        let backlog = {
            let mut state = self.lock();
            state.listener = Some(tx);
            std::mem::take(&mut state.pending)
        };
        for event in backlog {
            let frame = format!("event:receive\ndata:{event}\n\n");
            let _ = writer.write_all(frame.as_bytes()).await;
        }
        while let Some(Some(event)) = rx.recv().await {
            let frame = format!("event:receive\ndata:{event}\n\n");
            if writer.write_all(frame.as_bytes()).await.is_err() {
                return;
            }
        }
        let _ = writer.shutdown().await;
    }

    fn rpc(&self, body: &[u8]) -> String {
        let call: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let params = &call["params"];
        if params["account"] != ACCOUNT {
            return rpc_error(&call, &json!({"code": -1, "message": "Unknown account"}));
        }
        let result = match call["method"].as_str().unwrap_or_default() {
            "send" => return self.send(&call),
            "sendReaction" => {
                let target = format!(
                    "{}:{}",
                    params["targetAuthor"].as_str().unwrap_or_default(),
                    params["targetTimestamp"]
                );
                let emoji = params["emoji"].as_str().unwrap_or_default().to_owned();
                self.lock().reactions.push((target, emoji));
                json!({"timestamp": self.ts(), "results": [{"type": "SUCCESS"}]})
            }
            "sendTyping" => {
                self.lock().typing += 1;
                json!({})
            }
            "listGroups" => json!([{"id": "kitgroup==", "name": "Kit group"}]),
            "getAttachment" => {
                let id = params["id"].as_str().unwrap_or_default();
                let found = self
                    .lock()
                    .attachments
                    .iter()
                    .find(|(a, _)| a == id)
                    .cloned();
                match found {
                    Some((_, bytes)) => {
                        json!({"data": base64::engine::general_purpose::STANDARD.encode(bytes)})
                    }
                    None => {
                        return rpc_error(&call, &json!({"code": -1, "message": "no attachment"}));
                    }
                }
            }
            _ => {
                return rpc_error(
                    &call,
                    &json!({"code": -32601, "message": "Method not found"}),
                );
            }
        };
        response(
            200,
            &json!({"jsonrpc": "2.0", "id": call["id"], "result": result}).to_string(),
        )
    }

    fn send(&self, call: &Value) -> String {
        let params = &call["params"];
        let failure = self.lock().failures.pop_front();
        if let Some(failure) = failure {
            return match failure {
                Failure::RateLimited(secs) => rpc_error(
                    call,
                    &json!({"code": -5, "message": "Failed to send message due to rate limiting",
                            "data": {"response": {"timestamp": 0, "results": [
                                {"type": "RATE_LIMIT_FAILURE", "retryAfterSeconds": secs}]}}}),
                ),
                Failure::ServerError => response(500, ""),
                Failure::Markup | Failure::Forbidden => {
                    rpc_error(call, &json!({"code": -1, "message": "Unregistered user"}))
                }
            };
        }
        let text = params["message"].as_str().unwrap_or_default();
        if text.chars().count() > LIMIT {
            return rpc_error(call, &json!({"code": -1, "message": "Message too long"}));
        }
        let chat = params["groupId"].as_str().map_or_else(
            || {
                params["recipient"][0]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            },
            |g| format!("group.{g}"),
        );
        let mut state = self.lock();
        for path in params["attachments"].as_array().into_iter().flatten() {
            state
                .uploads
                .push(path.as_str().unwrap_or_default().to_owned());
        }
        if let Some(edited) = params["editTimestamp"].as_i64() {
            let id = format!("{ACCOUNT}:{edited}");
            if let Some(message) = state.sent.iter_mut().find(|m| m.message_id == id) {
                text.clone_into(&mut message.text);
            }
        }
        state.next += 1;
        let ts = state.next;
        state.sent.push(MockSent {
            chat_id: chat,
            message_id: format!("{ACCOUNT}:{ts}"),
            text: text.to_owned(),
            buttons: Vec::new(),
        });
        let result = json!({"timestamp": ts, "results": [{"type": "SUCCESS"}]});
        response(
            200,
            &json!({"jsonrpc": "2.0", "id": call["id"], "result": result}).to_string(),
        )
    }
}

fn rpc_error(call: &Value, error: &Value) -> String {
    response(
        200,
        &json!({"jsonrpc": "2.0", "id": call["id"], "error": error}).to_string(),
    )
}

impl MockPlatform for SignalMock {
    fn dm(&self) -> (String, String) {
        (USER.to_owned(), USER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        self.receive(&json!({"message": text}))
    }

    fn redeliver(&self) {
        let last = self.lock().last.clone();
        if let Some(event) = last {
            self.publish(event);
        }
    }

    fn drop_connection(&self) {
        if let Some(listener) = self.lock().listener.take() {
            let _ = listener.send(None);
        }
    }

    fn press(&self, _message_id: &str, _data: &str) {}

    fn fail_next(&self, failure: Failure) {
        self.lock().failures.push_back(failure);
    }

    fn reports_markup_errors(&self) -> bool {
        false
    }

    fn sent(&self) -> Vec<MockSent> {
        self.lock().sent.clone()
    }

    fn typing(&self) -> usize {
        self.lock().typing
    }

    fn reactions(&self) -> Vec<(String, String)> {
        self.lock().reactions.clone()
    }
}

/// The adapter, built for a fresh profile of the home at `home` pointing
/// at the mock daemon, and the mock: what
/// `prometheus plugins test-adapter signal` and the crate's tests run the
/// conformance kit on.
///
/// # Errors
///
/// The profile cannot be set up.
pub async fn kit(
    home: &std::path::Path,
) -> Result<
    (
        Arc<dyn pmt_gateway::messaging::Adapter>,
        Arc<dyn MockPlatform>,
    ),
    pmt_core::error::Error,
> {
    let mock = SignalMock::start().await;
    let setup =
        pmt_gateway::messaging::conformance::kit_setup(home, crate::PLATFORM, &mock.table(), &[])?;
    let adapter = crate::Signal::new(&setup)?;
    Ok((Arc::new(adapter), mock))
}
