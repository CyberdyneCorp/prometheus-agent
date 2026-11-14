//! ACP framing (spec: acp-adapter, Stdout carries only protocol frames):
//! newline-delimited JSON-RPC 2.0. Every frame the adapter emits goes
//! through one [`Out`] queue drained by one writer task, so the only
//! bytes on the protocol stream are whole frames, in order. The adapter's
//! own requests to the editor (permission cards, file reads, terminals)
//! wait for their responses here.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};
use tokio::io::{AsyncWrite, AsyncWriteExt as _};
use tokio::sync::{mpsc, oneshot};

/// JSON-RPC: the method does not exist.
pub(crate) const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC: invalid params.
pub(crate) const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC: invalid request.
pub(crate) const INVALID_REQUEST: i64 = -32600;
/// JSON-RPC: parse error.
pub(crate) const PARSE_ERROR: i64 = -32700;

/// One frame from the editor.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Incoming {
    /// A request to answer.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// A notification.
    Notification { method: String, params: Value },
    /// The answer to one of the adapter's requests.
    Response {
        id: Value,
        outcome: Result<Value, Value>,
    },
    /// Not a JSON-RPC frame: answered with this error.
    Invalid {
        id: Value,
        code: i64,
        message: String,
    },
}

/// Reads one line.
pub(crate) fn parse(line: &str) -> Incoming {
    let Ok(frame) = serde_json::from_str::<Value>(line) else {
        return Incoming::Invalid {
            id: Value::Null,
            code: PARSE_ERROR,
            message: "parse error".to_owned(),
        };
    };
    let id = frame.get("id").cloned();
    let params = frame.get("params").cloned().unwrap_or(Value::Null);
    match (frame.get("method").and_then(Value::as_str), id) {
        (Some(method), Some(id)) => Incoming::Request {
            id,
            method: method.to_owned(),
            params,
        },
        (Some(method), None) => Incoming::Notification {
            method: method.to_owned(),
            params,
        },
        (None, Some(id)) if frame.get("result").is_some() || frame.get("error").is_some() => {
            let outcome = match frame.get("error") {
                Some(error) => Err(error.clone()),
                None => Ok(frame.get("result").cloned().unwrap_or(Value::Null)),
            };
            Incoming::Response { id, outcome }
        }
        (None, id) => Incoming::Invalid {
            id: id.unwrap_or(Value::Null),
            code: INVALID_REQUEST,
            message: "invalid request".to_owned(),
        },
    }
}

/// A JSON-RPC error the adapter answers.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AcpError {
    pub(crate) code: i64,
    pub(crate) message: String,
}

impl AcpError {
    pub(crate) fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// `-32602` with `message`.
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }

    /// The daemon's error, as the editor sees it.
    pub(crate) fn of(err: &pmt_rpc::RpcError) -> Self {
        Self::new(err.code, err.message.clone())
    }
}

type Waiting = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, Value>>>>>;

/// The adapter's outgoing frames.
#[derive(Debug, Clone)]
pub(crate) struct Out {
    tx: mpsc::UnboundedSender<String>,
    waiting: Waiting,
    next: Arc<AtomicI64>,
}

impl Out {
    /// A queue and the receiving end its writer drains.
    pub(crate) fn new() -> (Self, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let out = Self {
            tx,
            waiting: Arc::default(),
            next: Arc::new(AtomicI64::new(1)),
        };
        (out, rx)
    }

    fn send(&self, frame: &Value) {
        let _ = self.tx.send(frame.to_string());
    }

    /// Sends notification `method`.
    pub(crate) fn notify(&self, method: &str, params: &Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Sends one `session/update` of editor session `session`.
    pub(crate) fn update(&self, session: &str, update: Value) {
        let mut params = serde_json::Map::new();
        params.insert("sessionId".to_owned(), Value::from(session));
        params.insert("update".to_owned(), update);
        self.notify("session/update", &Value::Object(params));
    }

    /// Answers request `id`.
    pub(crate) fn reply(&self, id: &Value, outcome: Result<Value, AcpError>) {
        let frame = match outcome {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(err) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": err.code, "message": err.message},
            }),
        };
        self.send(&frame);
    }

    /// Sends request `method` to the editor and waits for its answer;
    /// `Err` carries the editor's error (or `null` when the editor went
    /// away).
    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = oneshot::channel();
        self.waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, reply);
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        answer.await.unwrap_or(Err(Value::Null))
    }

    /// Hands a response to the request waiting for it; `false` when no
    /// request of the adapter has that id.
    pub(crate) fn settle(&self, id: &Value, outcome: Result<Value, Value>) -> bool {
        let Some(id) = id.as_i64() else {
            return false;
        };
        let waiting = self
            .waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        waiting.is_some_and(|reply| reply.send(outcome).is_ok())
    }
}

/// Writes every queued frame as one line, flushing each.
pub(crate) async fn write_frames<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut frames: mpsc::UnboundedReceiver<String>,
) {
    while let Some(mut line) = frames.recv().await {
        line.push('\n');
        if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Incoming, Out, parse};

    #[test]
    fn frames_are_classified_by_their_members() {
        let request = parse(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
        assert!(matches!(request, Incoming::Request { ref method, .. } if method == "initialize"));
        let note = parse(r#"{"jsonrpc":"2.0","method":"session/cancel","params":{}}"#);
        assert!(matches!(note, Incoming::Notification { .. }));
        let answer = parse(r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#);
        assert_eq!(
            answer,
            Incoming::Response {
                id: json!(7),
                outcome: Ok(json!({"ok": true}))
            }
        );
        assert!(matches!(
            parse("not json"),
            Incoming::Invalid { code: -32700, .. }
        ));
    }

    #[tokio::test]
    async fn a_request_waits_for_the_response_with_its_id() {
        let (out, mut rx) = Out::new();
        let asking = {
            let out = out.clone();
            pmt_core::spawn::spawn(async move { out.request("fs/read_text_file", json!({})).await })
        };
        let sent: serde_json::Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert!(!out.settle(&json!(999), Ok(json!(null))));
        assert!(out.settle(&sent["id"], Ok(json!({"content": "x"}))));
        assert_eq!(asking.await.unwrap(), Ok(json!({"content": "x"})));
    }
}
