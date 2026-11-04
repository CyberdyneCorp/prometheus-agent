//! The sidecar wire protocol (spec: python-bridge, Sidecar wire protocol):
//! JSON-RPC 2.0 messages, each framed by a 4-byte big-endian length, over
//! the sidecar's stdin (host to sidecar) and stdout (sidecar to host);
//! stderr carries logs. A frame above [`MAX_FRAME`] is never read.

use std::io::{self, Read, Write};

use serde_json::Value;

/// The protocol version both ends announce in `bridge.hello`.
pub const PROTOCOL_VERSION: u64 = 1;
/// Largest frame either end accepts.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// The closed set of host-to-sidecar methods.
pub const SIDECAR_METHODS: &[&str] = &[
    "bridge.hello",
    "bridge.ping",
    "bridge.shutdown",
    "plugin.load",
    "plugin.unload",
    "tool.invoke",
    "hook.invoke",
    "command.invoke",
    "provider.call",
    "event.deliver",
];
/// The one sidecar-to-host method.
pub const HOST_CALL: &str = "host.call";
/// JSON-RPC: method not found.
pub const UNKNOWN_METHOD: i64 = -32601;
/// The error data `code` of an oversized frame.
pub const FRAME_TOO_LARGE: &str = "frame_too_large";

/// Why a frame could not be read.
#[derive(Debug)]
pub enum FrameError {
    /// The announced length is above [`MAX_FRAME`].
    TooLarge(usize),
    /// The stream failed.
    Io(io::Error),
    /// The body is not JSON.
    Json(String),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(size) => write!(
                f,
                "{FRAME_TOO_LARGE}: a {size}-byte frame exceeds {MAX_FRAME} bytes"
            ),
            Self::Io(err) => write!(f, "{err}"),
            Self::Json(err) => write!(f, "invalid JSON frame: {err}"),
        }
    }
}

/// Writes one message.
///
/// # Errors
///
/// `InvalidInput` for a body above [`MAX_FRAME`], or the stream's error.
pub fn write_frame(writer: &mut impl Write, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message).map_err(io::Error::other)?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{FRAME_TOO_LARGE}: a {}-byte frame exceeds {MAX_FRAME} bytes",
                body.len()
            ),
        ));
    }
    let size = u32::try_from(body.len()).map_err(io::Error::other)?;
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&size.to_be_bytes());
    frame.extend_from_slice(&body);
    writer.write_all(&frame)?;
    writer.flush()
}

/// Reads one message; `Ok(None)` at a clean end of the stream.
///
/// # Errors
///
/// [`FrameError::TooLarge`] before reading the body of an oversized
/// frame, or a stream or JSON error.
pub fn read_frame(reader: &mut impl Read) -> Result<Option<Value>, FrameError> {
    let mut head = [0u8; 4];
    match reader.read_exact(&mut head) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(FrameError::Io(err)),
    }
    let size = u32::from_be_bytes(head) as usize;
    if size > MAX_FRAME {
        return Err(FrameError::TooLarge(size));
    }
    let mut body = vec![0u8; size];
    reader.read_exact(&mut body).map_err(FrameError::Io)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| FrameError::Json(e.to_string()))
}

/// A request.
#[must_use]
pub fn request(id: &Value, method: &str, params: &Value) -> Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// A successful answer.
#[must_use]
pub fn result(id: &Value, result: &Value) -> Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// An error answer with a machine-readable `data.code`.
#[must_use]
pub fn error(id: &Value, code: i64, message: &str, kind: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message, "data": {"code": kind}},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_with_a_big_endian_length() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &serde_json::json!({"a": 1})).unwrap();
        assert_eq!(&buffer[..4], &7u32.to_be_bytes());
        let read = read_frame(&mut buffer.as_slice()).unwrap().unwrap();
        assert_eq!(read["a"], 1);
        assert!(read_frame(&mut [].as_slice()).unwrap().is_none());
    }

    #[test]
    fn an_oversized_frame_is_rejected_before_its_body() {
        let size = u32::try_from(MAX_FRAME + 1).unwrap().to_be_bytes();
        let err = read_frame(&mut size.as_slice()).unwrap_err();
        assert!(matches!(err, FrameError::TooLarge(_)));
        assert!(err.to_string().starts_with("frame_too_large"));
        let big = Value::String("x".repeat(MAX_FRAME));
        let err = write_frame(&mut Vec::new(), &big).unwrap_err();
        assert!(err.to_string().starts_with("frame_too_large"));
    }
}
