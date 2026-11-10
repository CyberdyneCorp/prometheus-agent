//! JSON-RPC 2.0 frames: one message per WebSocket text frame (spec:
//! workspace-architecture, JSON-RPC framing and error codes). Parsing never
//! fails the connection: every malformed frame becomes the error reply the
//! spec names, with `id: null` when no id could be read.

use pmt_core::error::{Error, ErrorCode, codes};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::codes::{INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND, PARSE_ERROR, for_kind};

/// A request id: a number or a string (`srq-<12 hex>` for server requests).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    /// A numeric id.
    Num(i64),
    /// A string id.
    Str(String),
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Num(n) => write!(f, "{n}"),
            Self::Str(s) => f.write_str(s),
        }
    }
}

/// `error.data`: the taxonomy kind and stable dotted code, plus the hint
/// and, for params errors, the JSON pointer of the offending field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorData {
    /// The error kind (`budget_exceeded`, ...).
    pub kind: String,
    /// The stable dotted code (`rpc.unknown_field`, ...).
    pub code: String,
    /// What to do about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// The JSON pointer of the offending params field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    /// The code from [`crate::codes`].
    pub code: i64,
    /// One line for people.
    pub message: String,
    /// Kind, code, hint and path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Box<ErrorData>>,
}

impl RpcError {
    /// `err` under `code` instead of its kind's code.
    #[must_use]
    pub fn with_code(code: i64, err: &Error) -> Self {
        Self {
            code,
            message: err.message().to_owned(),
            data: Some(Box::new(ErrorData {
                kind: err.kind().as_str().to_owned(),
                code: err.code().code.to_owned(),
                hint: err.hint().map(str::to_owned),
                path: None,
            })),
        }
    }

    /// An application error under its kind's code.
    #[must_use]
    pub fn from_error(err: &Error) -> Self {
        // A handler's own params check answers like the contract check.
        let code = if [
            codes::RPC_INVALID_PARAMS.code,
            codes::RPC_UNKNOWN_FIELD.code,
        ]
        .contains(&err.code().code)
        {
            INVALID_PARAMS
        } else {
            for_kind(err.kind())
        };
        Self::with_code(code, err)
    }

    fn of(code: i64, catalog: &'static ErrorCode, message: impl Into<String>) -> Self {
        Self::with_code(code, &Error::new(catalog, message))
    }

    /// `-32700` for a frame that is not JSON.
    #[must_use]
    pub fn parse(detail: &str) -> Self {
        Self::of(
            PARSE_ERROR,
            &codes::RPC_PARSE,
            format!("parse error: {detail}"),
        )
    }

    /// `-32600` for a frame that is not a request object.
    #[must_use]
    pub fn invalid_request(why: &str) -> Self {
        Self::of(
            INVALID_REQUEST,
            &codes::RPC_INVALID_REQUEST,
            format!("invalid request: {why}"),
        )
    }

    /// `-32601 unknown method: <method>`.
    #[must_use]
    pub fn unknown_method(method: &str) -> Self {
        Self::of(
            METHOD_NOT_FOUND,
            &codes::RPC_UNKNOWN_METHOD,
            format!("unknown method: {method}"),
        )
    }

    /// `-32602` naming `path`; `unknown` for an undeclared field.
    #[must_use]
    pub fn invalid_params(path: &str, unknown: bool, message: &str) -> Self {
        let catalog = if unknown {
            &codes::RPC_UNKNOWN_FIELD
        } else {
            &codes::RPC_INVALID_PARAMS
        };
        let mut err = Self::of(INVALID_PARAMS, catalog, message);
        if let Some(data) = &mut err.data {
            data.path = Some(path.to_owned());
        }
        err
    }

    /// The taxonomy kind carried in `data`, if any.
    #[must_use]
    pub fn kind(&self) -> Option<&str> {
        self.data.as_ref().map(|d| d.kind.as_str())
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// One parsed frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// A call expecting a response.
    Request {
        /// Its id.
        id: Id,
        /// The method.
        method: String,
        /// Its params (an object; `{}` when absent).
        params: Value,
        /// The caller's W3C `traceparent`, from the request's
        /// `meta.traceparent` (spec: observability, Trace context
        /// propagation).
        traceparent: Option<String>,
    },
    /// A call without an id (events from the daemon; ignored by it).
    Notification {
        /// The method or event name.
        method: String,
        /// Its params (an object; `{}` when absent).
        params: Value,
    },
    /// A response to a request the reader sent.
    Response {
        /// The request it answers.
        id: Id,
        /// The result or the error.
        outcome: Result<Value, RpcError>,
    },
}

/// A frame the reader must answer with an error, and the id to answer
/// (`None` serializes as `null`).
pub type Rejected = (Option<Id>, RpcError);

/// Parses one text frame.
///
/// # Errors
///
/// The error reply for a malformed frame.
pub fn parse(text: &str) -> Result<Incoming, Rejected> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| (None, RpcError::parse(&e.to_string())))?;
    let Value::Object(mut object) = value else {
        return Err((None, RpcError::invalid_request("expected a JSON object")));
    };
    let id = match object.remove("id") {
        None | Some(Value::Null) => None,
        Some(raw) => Some(serde_json::from_value::<Id>(raw).map_err(|_| {
            (
                None,
                RpcError::invalid_request("id must be a number or a string"),
            )
        })?),
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err((id, RpcError::invalid_request("jsonrpc must be \"2.0\"")));
    }
    let traceparent = object
        .get("meta")
        .and_then(|meta| meta.get("traceparent"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    match object.remove("method") {
        Some(Value::String(method)) => call(id, method, object.remove("params"), traceparent),
        Some(_) => Err((id, RpcError::invalid_request("method must be a string"))),
        None => response(id, &mut object),
    }
}

fn call(
    id: Option<Id>,
    method: String,
    params: Option<Value>,
    traceparent: Option<String>,
) -> Result<Incoming, Rejected> {
    let params = match params {
        None => Value::Object(Map::new()),
        Some(params @ Value::Object(_)) => params,
        Some(_) => {
            return Err((
                id,
                RpcError::invalid_params("", false, "params must be an object"),
            ));
        }
    };
    Ok(match id {
        Some(id) => Incoming::Request {
            id,
            method,
            params,
            traceparent,
        },
        None => Incoming::Notification { method, params },
    })
}

fn response(id: Option<Id>, object: &mut Map<String, Value>) -> Result<Incoming, Rejected> {
    let Some(id) = id else {
        return Err((None, RpcError::invalid_request("missing method")));
    };
    let outcome = match (object.remove("result"), object.remove("error")) {
        (Some(result), None) => Ok(result),
        (None, Some(error)) => Err(serde_json::from_value::<RpcError>(error).map_err(|_| {
            (
                Some(id.clone()),
                RpcError::invalid_request("malformed error object"),
            )
        })?),
        _ => {
            return Err((Some(id), RpcError::invalid_request("missing method")));
        }
    };
    Ok(Incoming::Response { id, outcome })
}

/// A request frame.
#[must_use]
pub fn request(id: &Id, method: &str, params: &Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

/// A notification (event) frame.
#[must_use]
pub fn notification(method: &str, params: &Value) -> String {
    json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string()
}

/// A success response frame.
#[must_use]
pub fn result(id: &Id, result: &Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

/// An error response frame (`id: null` when `id` is `None`).
#[must_use]
pub fn error(id: Option<&Id>, error: &RpcError) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": error}).to_string()
}

#[cfg(test)]
mod tests {
    use pmt_core::error::{Error, codes};
    use serde_json::json;

    use super::{Id, Incoming, RpcError, parse};

    fn rejected(text: &str) -> (Option<Id>, i64) {
        let (id, err) = parse(text).unwrap_err();
        (id, err.code)
    }

    #[test]
    fn malformed_frames_get_the_spec_codes() {
        assert_eq!(rejected("not json"), (None, -32_700));
        assert_eq!(rejected("[1, 2]"), (None, -32_600));
        assert_eq!(rejected("\"x\""), (None, -32_600));
        assert_eq!(
            rejected(r#"{"jsonrpc":"2.0","id":3,"method":"rpc.ping","params":[1]}"#),
            (Some(Id::Num(3)), -32_602)
        );
        assert_eq!(
            rejected(r#"{"jsonrpc":"1.0","id":"a","method":"rpc.ping"}"#),
            (Some(Id::Str("a".into())), -32_600)
        );
        assert_eq!(
            rejected(r#"{"jsonrpc":"2.0","id":4}"#),
            (Some(Id::Num(4)), -32_600)
        );
    }

    #[test]
    fn requests_notifications_and_responses_are_told_apart() {
        let request = parse(r#"{"jsonrpc":"2.0","id":1,"method":"rpc.ping"}"#).unwrap();
        assert_eq!(
            request,
            Incoming::Request {
                id: Id::Num(1),
                method: "rpc.ping".into(),
                params: json!({}),
                traceparent: None,
            }
        );
        let event = parse(r#"{"jsonrpc":"2.0","method":"x","params":{"a":1}}"#).unwrap();
        assert!(matches!(event, Incoming::Notification { .. }));
        let answer =
            parse(r#"{"jsonrpc":"2.0","id":"srq-0123456789ab","result":{"decision":"once"}}"#)
                .unwrap();
        assert!(matches!(answer, Incoming::Response { outcome: Ok(_), .. }));
    }

    #[test]
    fn application_errors_carry_kind_and_code() {
        let err = Error::new(&codes::SESSION_BUSY, "busy");
        let rpc = RpcError::from_error(&err);
        assert_eq!(rpc.code, -32_004);
        let data = rpc.data.unwrap();
        assert_eq!(data.kind, "conflict");
        assert_eq!(data.code, "session.busy");
        let unknown = RpcError::invalid_params("/sesion_id", true, "unknown field");
        assert_eq!(unknown.data.unwrap().path.as_deref(), Some("/sesion_id"));
    }
}
