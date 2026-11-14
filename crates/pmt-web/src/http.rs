//! Requests and answers of the dashboard listener: the request head with a
//! body read only on demand (bounded), and JSON, text and empty answers.

use std::convert::Infallible;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt as _, Empty, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{CONTENT_TYPE, HeaderValue};
use hyper::{HeaderMap, Method, Response, StatusCode};
use serde_json::{Value, json};

/// The answer body type.
pub(crate) type Body = BoxBody<Bytes, Infallible>;
/// An answer.
pub(crate) type Reply = Response<Body>;

/// The largest JSON body an API call reads (uploads have their own cap).
pub(crate) const MAX_JSON_BODY: usize = 1024 * 1024;

/// A request head; the body stays unread until a handler asks for it.
#[derive(Debug)]
pub(crate) struct Req {
    pub method: Method,
    /// The path below the base path, always starting with `/`.
    pub path: String,
    pub query: Option<String>,
    pub headers: HeaderMap,
}

impl Req {
    /// A header's value as text.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// A query parameter, percent-decoded.
    pub(crate) fn param(&self, name: &str) -> Option<String> {
        let query = self.query.as_deref()?;
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }

    /// The path's segments (`/api/profiles/x` -> `["api", "profiles", "x"]`).
    pub(crate) fn segments(&self) -> Vec<&str> {
        self.path
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect()
    }
}

/// Reads at most `limit` bytes of `body`; `None` when it is larger or broken.
pub(crate) async fn read_body(body: Incoming, limit: usize) -> Option<Bytes> {
    Limited::new(body, limit)
        .collect()
        .await
        .ok()
        .map(http_body_util::Collected::to_bytes)
}

/// Reads a JSON object body (an empty body is `{}`).
pub(crate) async fn read_json(body: Incoming) -> Result<Value, Reply> {
    let Some(bytes) = read_body(body, MAX_JSON_BODY).await else {
        return Err(error(
            413,
            "body_too_large",
            "the request body is too large",
        ));
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(json!({}));
    }
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(value @ Value::Object(_)) => Ok(value),
        Ok(_) => Err(error(400, "invalid_body", "the body must be a JSON object")),
        Err(e) => Err(error(
            400,
            "invalid_json",
            &format!("the body is not JSON: {e}"),
        )),
    }
}

fn status(code: u16) -> StatusCode {
    StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// A response with `body` and `content_type`.
pub(crate) fn bytes(code: u16, content_type: &str, body: impl Into<Bytes>) -> Reply {
    let mut reply = Response::new(Full::new(body.into()).boxed());
    *reply.status_mut() = status(code);
    if let Ok(value) = HeaderValue::from_str(content_type) {
        reply.headers_mut().insert(CONTENT_TYPE, value);
    }
    reply
}

/// A JSON answer.
pub(crate) fn json_reply(code: u16, body: &Value) -> Reply {
    bytes(
        code,
        "application/json",
        serde_json::to_vec(body).unwrap_or_default(),
    )
}

/// `{"error": <code>, "message": <message>}`.
pub(crate) fn error(code: u16, error: &str, message: &str) -> Reply {
    json_reply(code, &json!({"error": error, "message": message}))
}

/// An answer with no body.
pub(crate) fn empty(code: u16) -> Reply {
    let mut reply = Response::new(Empty::new().boxed());
    *reply.status_mut() = status(code);
    reply
}

/// A `303 See Other` to `location`.
pub(crate) fn redirect(location: &str) -> Reply {
    let mut reply = empty(303);
    if let Ok(value) = HeaderValue::from_str(location) {
        reply.headers_mut().insert(hyper::header::LOCATION, value);
    }
    reply
}

/// `404 {"error": "not_found"}`.
pub(crate) fn not_found() -> Reply {
    error(404, "not_found", "no such endpoint")
}

/// The answer of a failed operation: its HTTP status from the error kind.
pub(crate) fn from_error(err: &pmt_core::error::Error) -> Reply {
    use pmt_core::error::ErrorKind;
    let code = match err.kind() {
        ErrorKind::Usage | ErrorKind::InvalidInput | ErrorKind::Config => 400,
        ErrorKind::NotFound => 404,
        ErrorKind::Auth => 401,
        ErrorKind::PermissionDenied => 403,
        ErrorKind::Conflict => 409,
        ErrorKind::Unavailable => 503,
        _ => 500,
    };
    let mut body = json!({"error": err.code().code, "message": err.message()});
    if let Some(hint) = err.hint() {
        body["hint"] = hint.into();
    }
    json_reply(code, &body)
}
