//! Requests and answers of the API server: a request with its body read
//! (bounded), JSON and OpenAI-style error answers, and server-sent event
//! streams that send a `: keepalive` comment after 10 seconds of silence.

use std::convert::Infallible;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt as _, Full, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::{HeaderMap, Method, Response};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// The answer body type.
pub(crate) type Body = BoxBody<Bytes, Infallible>;
/// An answer.
pub(crate) type Reply = Response<Body>;

/// The largest request body (images arrive inline as data URLs).
pub(crate) const MAX_BODY: usize = 32 * 1024 * 1024;
/// Silence after which an event stream sends a keepalive comment.
pub(crate) const KEEPALIVE: Duration = Duration::from_secs(10);

/// A request with its body read.
#[derive(Debug)]
pub(crate) struct Req {
    pub method: Method,
    /// The path below the profile prefix (`/v1/models`).
    pub path: String,
    pub query: Option<String>,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Req {
    /// The request line and headers, with an empty body: the guard and the
    /// bearer check run on these before any body is read.
    pub(crate) fn head(parts: hyper::http::request::Parts) -> Self {
        Self {
            method: parts.method,
            path: parts.uri.path().to_owned(),
            query: parts.uri.query().map(str::to_owned),
            headers: parts.headers,
            body: Bytes::new(),
        }
    }

    /// Reads `body` (at most [`MAX_BODY`]) into the request; `None` when it
    /// is larger or broken.
    pub(crate) async fn with_body(mut self, body: Incoming) -> Option<Self> {
        self.body = Limited::new(body, MAX_BODY)
            .collect()
            .await
            .ok()?
            .to_bytes();
        Some(self)
    }

    /// A header's value as text.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// The first of `names` present.
    pub(crate) fn header_any(&self, names: &[&str]) -> Option<&str> {
        names.iter().find_map(|n| self.header(n))
    }

    /// A query parameter.
    pub(crate) fn param(&self, name: &str) -> Option<String> {
        self.query.as_deref()?.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (key == name).then(|| percent_decode(value))
        })
    }

    /// The body as a JSON object; `{}` for an empty body.
    pub(crate) fn json(&self) -> Result<Value, Reply> {
        if self.body.iter().all(u8::is_ascii_whitespace) {
            return Ok(json!({}));
        }
        match serde_json::from_slice::<Value>(&self.body) {
            Ok(value @ Value::Object(_)) => Ok(value),
            Ok(_) => Err(error(
                400,
                "invalid_request_error",
                "invalid_body",
                "the body must be a JSON object",
            )),
            Err(e) => Err(error(
                400,
                "invalid_request_error",
                "invalid_json",
                &format!("the body is not JSON: {e}"),
            )),
        }
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = bytes.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(&String::from_utf8_lossy(hex), 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A body of `bytes`.
pub(crate) fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into()).boxed()
}

/// A JSON answer.
pub(crate) fn json_reply(status: u16, value: &Value) -> Reply {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(full(value.to_string()))
        .unwrap_or_else(|_| Response::new(full(Bytes::new())))
}

/// An OpenAI-style error answer: `{"error": {"message", "type", "code"}}`.
pub(crate) fn error(status: u16, kind: &str, code: &str, message: &str) -> Reply {
    json_reply(
        status,
        &json!({"error": {"message": message, "type": kind, "code": code, "param": null}}),
    )
}

/// 400 `invalid_request_error`.
pub(crate) fn bad_request(code: &str, message: &str) -> Reply {
    error(400, "invalid_request_error", code, message)
}

/// 404 `not_found`.
pub(crate) fn not_found(what: &str) -> Reply {
    error(
        404,
        "invalid_request_error",
        "not_found",
        &format!("{what} not found"),
    )
}

/// 409 with `code`.
pub(crate) fn conflict(code: &str, message: &str) -> Reply {
    error(409, "invalid_request_error", code, message)
}

/// 503 with `Retry-After`.
pub(crate) fn unavailable(code: &str, message: &str, retry_after: u64) -> Reply {
    with_header(
        error(503, "server_error", code, message),
        "retry-after",
        &retry_after.to_string(),
    )
}

/// The answer of a daemon error: storage contention is a 503 to retry,
/// anything else a 500 naming only its stable code.
pub(crate) fn from_error(err: &pmt_core::Error) -> Reply {
    if err.code().code == pmt_core::error::codes::STORE_BUSY.code {
        return unavailable(
            "storage_unavailable",
            "the profile's store is busy; retry",
            2,
        );
    }
    match err.kind() {
        pmt_core::error::ErrorKind::NotFound => not_found("the resource"),
        pmt_core::error::ErrorKind::BudgetExceeded => {
            error(429, "insufficient_quota", err.code().code, err.message())
        }
        pmt_core::error::ErrorKind::Conflict | pmt_core::error::ErrorKind::Unavailable => {
            unavailable(err.code().code, err.message(), 5)
        }
        pmt_core::error::ErrorKind::InvalidInput
        | pmt_core::error::ErrorKind::Usage
        | pmt_core::error::ErrorKind::Config => bad_request(err.code().code, err.message()),
        _ => {
            tracing::warn!(code = err.code().code, "api request failed: {err}");
            error(500, "server_error", err.code().code, "the request failed")
        }
    }
}

/// `reply` with one more header.
pub(crate) fn with_header(mut reply: Reply, name: &str, value: &str) -> Reply {
    if let (Ok(name), Ok(value)) = (
        hyper::header::HeaderName::from_bytes(name.as_bytes()),
        hyper::header::HeaderValue::from_str(value),
    ) {
        reply.headers_mut().insert(name, value);
    }
    reply
}

/// One server-sent event.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Sse {
    /// `id:` (Runs events).
    pub id: Option<u64>,
    /// `event:`; `None` for a plain `data:` frame.
    pub event: Option<String>,
    /// The data line.
    pub data: String,
}

impl Sse {
    /// A plain `data:` frame of `value`.
    pub(crate) fn data(value: &Value) -> Self {
        Self {
            id: None,
            event: None,
            data: value.to_string(),
        }
    }

    /// A named event.
    pub(crate) fn named(event: &str, value: &Value) -> Self {
        Self {
            id: None,
            event: Some(event.to_owned()),
            data: value.to_string(),
        }
    }

    fn encode(&self) -> String {
        use std::fmt::Write as _;
        let mut text = String::new();
        if let Some(id) = self.id {
            let _ = writeln!(text, "id: {id}");
        }
        if let Some(event) = &self.event {
            let _ = writeln!(text, "event: {event}");
        }
        for line in self.data.split('\n') {
            let _ = writeln!(text, "data: {line}");
        }
        text.push('\n');
        text
    }
}

/// An event-stream answer fed by the returned sender; the stream ends
/// when the sender is dropped.
pub(crate) fn sse_reply() -> (mpsc::UnboundedSender<Sse>, Reply) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Sse>();
    let (out_tx, out_rx) = mpsc::channel::<Bytes>(64);
    pmt_core::spawn::spawn(async move {
        loop {
            let frame = match tokio::time::timeout(KEEPALIVE, rx.recv()).await {
                Ok(Some(event)) => Bytes::from(event.encode()),
                Ok(None) => return,
                Err(_) => Bytes::from_static(b": keepalive\n\n"),
            };
            if out_tx.send(frame).await.is_err() {
                return;
            }
        }
    });
    let stream = futures_util::stream::unfold(out_rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|bytes| (Ok::<_, Infallible>(Frame::data(bytes)), rx))
    });
    let reply = Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(http_body_util::BodyExt::boxed(StreamBody::new(stream)))
        .unwrap_or_else(|_| Response::new(full(Bytes::new())));
    (tx, reply)
}

#[cfg(test)]
mod tests {
    use super::{Sse, percent_decode};

    #[test]
    fn events_encode_with_id_name_and_multiline_data() {
        let event = Sse {
            id: Some(42),
            event: Some("message.delta".into()),
            data: "a\nb".into(),
        };
        assert_eq!(
            event.encode(),
            "id: 42\nevent: message.delta\ndata: a\ndata: b\n\n"
        );
    }

    #[test]
    fn query_values_are_decoded() {
        assert_eq!(percent_decode("webui%3Auser-42+x"), "webui:user-42 x");
    }
}
