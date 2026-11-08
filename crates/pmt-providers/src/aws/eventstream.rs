//! The AWS event-stream framing (`application/vnd.amazon.eventstream`) of
//! Bedrock `ConverseStream` responses: length-prefixed binary frames
//! with CRC-32 checks, typed headers and a JSON payload. Each frame
//! becomes one [`SseEvent`] named by its `:event-type`, or
//! `exception.<type>` for an exception or error frame, so the Converse
//! decoder reads the same events as the SSE-based decoders.

use std::collections::BTreeMap;

use crate::sse::SseEvent;

/// Total length, headers length and prelude CRC.
const PRELUDE: usize = 12;
/// The message CRC after the payload.
const TRAILER: usize = 4;
/// Frames larger than this are refused (AWS caps them at 16 MiB).
const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Buffers bytes and yields complete frames.
#[derive(Debug, Default)]
pub(crate) struct Parser {
    buffer: Vec<u8>,
}

impl Parser {
    /// Feeds `bytes` and returns the events of the frames they complete.
    ///
    /// # Errors
    ///
    /// A frame whose length or checksums are wrong.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, String> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while self.buffer.len() >= PRELUDE {
            let total = be32(&self.buffer[0..4]);
            if !(PRELUDE + TRAILER..=MAX_FRAME).contains(&total) {
                return Err(format!("event-stream frame of {total} bytes"));
            }
            if self.buffer.len() < total {
                break;
            }
            let frame: Vec<u8> = self.buffer.drain(..total).collect();
            events.push(decode(&frame)?);
        }
        Ok(events)
    }
}

fn be32(bytes: &[u8]) -> usize {
    let mut word = [0; 4];
    word.copy_from_slice(&bytes[..4]);
    u32::from_be_bytes(word) as usize
}

fn decode(frame: &[u8]) -> Result<SseEvent, String> {
    let total = frame.len();
    if crc(&frame[..8]) != be32(&frame[8..12]) {
        return Err("event-stream prelude checksum mismatch".to_owned());
    }
    if crc(&frame[..total - TRAILER]) != be32(&frame[total - TRAILER..]) {
        return Err("event-stream message checksum mismatch".to_owned());
    }
    let headers_end = PRELUDE + be32(&frame[4..8]);
    if headers_end > total - TRAILER {
        return Err("event-stream headers overrun the frame".to_owned());
    }
    let headers = headers(&frame[PRELUDE..headers_end])?;
    let payload = String::from_utf8_lossy(&frame[headers_end..total - TRAILER]).into_owned();
    let header = |name: &str| headers.get(name).cloned().unwrap_or_default();
    Ok(match header(":message-type").as_str() {
        "exception" => SseEvent {
            event: Some(format!("exception.{}", header(":exception-type"))),
            data: payload,
        },
        "error" => SseEvent {
            event: Some(format!("exception.{}", header(":error-code"))),
            data: serde_json::json!({"message": header(":error-message")}).to_string(),
        },
        _ => SseEvent {
            event: Some(header(":event-type")),
            data: payload,
        },
    })
}

fn crc(bytes: &[u8]) -> usize {
    crc32fast::hash(bytes) as usize
}

/// The string-valued headers (the only ones Bedrock frames carry that
/// matter); other value types are skipped by their encoded size.
fn headers(mut bytes: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let truncated = || "event-stream header truncated".to_owned();
    let mut out = BTreeMap::new();
    while let Some((&name_len, rest)) = bytes.split_first() {
        let name_len = usize::from(name_len);
        let name = rest.get(..name_len).ok_or_else(truncated)?;
        let (&kind, rest) = rest[name_len..].split_first().ok_or_else(truncated)?;
        let size = match kind {
            0 | 1 => 0,
            2 => 1,
            3 => 2,
            4 => 4,
            5 | 8 => 8,
            9 => 16,
            6 | 7 => {
                let len = rest.get(..2).ok_or_else(truncated)?;
                2 + usize::from(u16::from_be_bytes([len[0], len[1]]))
            }
            other => return Err(format!("event-stream header type {other}")),
        };
        let value = rest.get(..size).ok_or_else(truncated)?;
        if kind == 7 {
            out.insert(
                String::from_utf8_lossy(name).into_owned(),
                String::from_utf8_lossy(&value[2..]).into_owned(),
            );
        }
        bytes = &rest[size..];
    }
    Ok(out)
}

/// One frame with string headers (tests and fixtures).
#[cfg(test)]
pub(crate) fn frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::new();
    for (name, value) in headers {
        encoded.push(u8::try_from(name.len()).unwrap());
        encoded.extend_from_slice(name.as_bytes());
        encoded.push(7);
        encoded.extend_from_slice(&u16::try_from(value.len()).unwrap().to_be_bytes());
        encoded.extend_from_slice(value.as_bytes());
    }
    let total = PRELUDE + encoded.len() + payload.len() + TRAILER;
    let mut out = Vec::new();
    out.extend_from_slice(&u32::try_from(total).unwrap().to_be_bytes());
    out.extend_from_slice(&u32::try_from(encoded.len()).unwrap().to_be_bytes());
    out.extend_from_slice(&crc32fast::hash(&out).to_be_bytes());
    out.extend_from_slice(&encoded);
    out.extend_from_slice(payload);
    out.extend_from_slice(&crc32fast::hash(&out).to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_split_across_chunks_decode_in_order() {
        let mut bytes = frame(
            &[(":message-type", "event"), (":event-type", "messageStart")],
            br#"{"role":"assistant"}"#,
        );
        bytes.extend(frame(
            &[
                (":message-type", "exception"),
                (":exception-type", "throttlingException"),
            ],
            br#"{"message":"slow down"}"#,
        ));
        let mut parser = Parser::default();
        let (head, tail) = bytes.split_at(7);
        assert!(parser.feed(head).unwrap().is_empty());
        let events = parser.feed(tail).unwrap();
        assert_eq!(events[0].event.as_deref(), Some("messageStart"));
        assert_eq!(events[0].data, r#"{"role":"assistant"}"#);
        assert_eq!(
            events[1].event.as_deref(),
            Some("exception.throttlingException")
        );
    }

    #[test]
    fn a_corrupted_frame_is_refused() {
        let mut bytes = frame(&[(":event-type", "messageStop")], b"{}");
        let last = bytes.len() - 5;
        bytes[last] ^= 1;
        assert!(Parser::default().feed(&bytes).is_err());
    }
}
