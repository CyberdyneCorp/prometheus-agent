//! The two things the shell itself says to a connection's daemon: one
//! HTTP/1.1 request (health, pairing, the profile list) and a short client
//! protocol exchange (`rpc.hello` and one call, for Test and Update all
//! instances). Both run over a stream [`super::tls::dial`] or a tunnel
//! opened, so pinning and SSH apply to them as to the page's own dials.

use std::fmt::Write as _;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use super::tls::BoxStream;

/// How long a request or a call may take.
pub const TIMEOUT: Duration = Duration::from_secs(20);
/// The largest answer read.
const MAX_ANSWER: u64 = 4 << 20;

/// An HTTP answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// The status code.
    pub status: u16,
    /// The body.
    pub body: Vec<u8>,
}

impl Reply {
    /// The body as JSON (`null` when it is not).
    #[must_use]
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

/// One request on `stream` (the connection closes after it).
///
/// # Errors
///
/// The stream failed, timed out, or the answer is not HTTP.
pub async fn request(
    mut stream: BoxStream,
    host: &str,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<&Value>,
) -> Result<Reply, String> {
    let body = body.map(Value::to_string).unwrap_or_default();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if !body.is_empty() {
        head.push_str("Content-Type: application/json\r\n");
    }
    if let Some(token) = bearer {
        let _ = write!(head, "Authorization: Bearer {token}\r\n");
    }
    head.push_str("\r\n");
    let exchange = async {
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(body.as_bytes()).await?;
        stream.flush().await?;
        let mut raw = Vec::new();
        stream.take(MAX_ANSWER).read_to_end(&mut raw).await?;
        Ok::<_, std::io::Error>(raw)
    };
    let raw = tokio::time::timeout(TIMEOUT, exchange)
        .await
        .map_err(|_| "the request timed out".to_owned())?
        .map_err(|e| e.to_string())?;
    parse(&raw)
}

fn parse(raw: &[u8]) -> Result<Reply, String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("not an HTTP answer")?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("not an HTTP answer")?;
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let rest = &raw[split + 4..];
    let body = if chunked {
        dechunk(rest)
    } else {
        rest.to_vec()
    };
    Ok(Reply { status, body })
}

fn dechunk(mut rest: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    while let Some(end) = rest.windows(2).position(|w| w == b"\r\n") {
        let size = std::str::from_utf8(&rest[..end])
            .ok()
            .and_then(|s| usize::from_str_radix(s.split(';').next().unwrap_or("").trim(), 16).ok())
            .unwrap_or(0);
        let start = end + 2;
        if size == 0 || rest.len() < start + size {
            break;
        }
        body.extend_from_slice(&rest[start..start + size]);
        rest = rest.get(start + size + 2..).unwrap_or_default();
    }
    body
}

/// A client protocol socket opened by the shell.
pub struct Socket {
    ws: tokio_tungstenite::WebSocketStream<BoxStream>,
    next: u64,
}

/// What opening a socket needs.
pub struct Upgrade<'a> {
    /// `ws://<authority><path>` as the server sees it.
    pub url: &'a str,
    /// The subprotocols offered (protocol version, maybe a ticket).
    pub protocols: &'a [String],
    /// The device credential, for a dashboard listener.
    pub bearer: Option<&'a str>,
}

impl Socket {
    /// Upgrades `stream` and says `rpc.hello` as a desktop client.
    ///
    /// # Errors
    ///
    /// The upgrade or the hello failed (a refused credential closes with
    /// 4401).
    pub async fn open(stream: BoxStream, upgrade: &Upgrade<'_>) -> Result<Self, String> {
        let mut request = upgrade
            .url
            .into_client_request()
            .map_err(|e| e.to_string())?;
        let headers = request.headers_mut();
        let offered =
            HeaderValue::from_str(&upgrade.protocols.join(", ")).map_err(|e| e.to_string())?;
        headers.insert("Sec-WebSocket-Protocol", offered);
        if let Some(token) = upgrade.bearer {
            let value =
                HeaderValue::from_str(&format!("Bearer {token}")).map_err(|e| e.to_string())?;
            headers.insert("Authorization", value);
        }
        let (ws, _) =
            tokio::time::timeout(TIMEOUT, tokio_tungstenite::client_async(request, stream))
                .await
                .map_err(|_| "the WebSocket handshake timed out".to_owned())?
                .map_err(|e| format!("the WebSocket handshake failed: {e}"))?;
        let mut socket = Self { ws, next: 0 };
        let protocol = pmt_core::protocol::RPC_PROTOCOL;
        socket
            .call(
                "rpc.hello",
                json!({
                    "client": "desktop",
                    "client_version": env!("CARGO_PKG_VERSION"),
                    "protocol": {"major": protocol.major, "minor": protocol.minor},
                    "capabilities": {"server_requests": false, "events": []},
                }),
            )
            .await?;
        Ok(socket)
    }

    /// One call; its result.
    ///
    /// # Errors
    ///
    /// The server's error message, or the socket closed (with its code).
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.ws
            .send(Message::text(frame.to_string()))
            .await
            .map_err(|e| e.to_string())?;
        tokio::time::timeout(TIMEOUT, self.answer(id))
            .await
            .map_err(|_| format!("{method} timed out"))?
    }

    async fn answer(&mut self, id: u64) -> Result<Value, String> {
        while let Some(frame) = self.ws.next().await {
            match frame.map_err(|e| e.to_string())? {
                Message::Text(text) => {
                    let value: Value = serde_json::from_str(text.as_str()).unwrap_or(Value::Null);
                    if value["id"] != json!(id) || value.get("method").is_some() {
                        continue;
                    }
                    if let Some(error) = value.get("error") {
                        let message = error["message"].as_str().unwrap_or("refused");
                        return Err(message.to_owned());
                    }
                    return Ok(value["result"].clone());
                }
                Message::Close(frame) => {
                    let code = frame.map_or(1005, |f| u16::from(f.code));
                    return Err(closed(code));
                }
                _ => {}
            }
        }
        Err("the socket ended".to_owned())
    }

    /// Closes the socket.
    pub async fn close(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(2), self.ws.close(None)).await;
    }
}

fn closed(code: u16) -> String {
    match code {
        4401 => "the server refused the credential (4401): pair this device again".to_owned(),
        4403 => "the server refused this connection (4403)".to_owned(),
        other => format!("the server closed the socket ({other})"),
    }
}
