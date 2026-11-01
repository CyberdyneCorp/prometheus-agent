//! Incremental Server-Sent Events parsing (the transport of every native
//! wire format's stream).

/// One event: its `event:` name (if any) and its joined `data:` lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Buffers bytes and yields complete events.
#[derive(Debug, Default)]
pub(crate) struct SseParser {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    /// Feeds `bytes` and returns the events they complete.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if let Some(event) = self.line(line) {
                events.push(event);
            }
        }
        events
    }

    /// Flushes a final event not followed by a blank line.
    pub(crate) fn finish(&mut self) -> Option<SseEvent> {
        if !self.buffer.is_empty() {
            let rest = String::from_utf8_lossy(&std::mem::take(&mut self.buffer)).into_owned();
            if let Some(event) = self.line(rest.trim_end_matches('\r')) {
                return Some(event);
            }
        }
        self.dispatch()
    }

    fn line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        if self.data.is_empty() {
            self.event = None;
            return None;
        }
        Some(SseEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data).join("\n"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_split_across_chunks_and_crlf() {
        let mut p = SseParser::default();
        assert!(p.feed(b"event: a\r\ndata: {\"x\"").is_empty());
        let events = p.feed(b":1}\r\n\r\n: comment\ndata: one\ndata: two\n\ndata: [DONE]");
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: Some("a".into()),
                    data: "{\"x\":1}".into()
                },
                SseEvent {
                    event: None,
                    data: "one\ntwo".into()
                },
            ]
        );
        assert_eq!(
            p.finish(),
            Some(SseEvent {
                event: None,
                data: "[DONE]".into()
            })
        );
    }
}
