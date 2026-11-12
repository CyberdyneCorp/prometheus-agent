//! Streaming JSONL output (spec: cli, Streaming JSONL output): with
//! `--output-format stream-json` a one-shot run writes each of its
//! session's trajectory events to stdout as it happens, one JSON object
//! per line in the trajectory's own line shape (observability, Trajectory
//! event types), flushed after every line, and ends with one `result`
//! line carrying the session id, the answer, the usage and the exit
//! reason. Payloads are redacted like the trajectory file's.

use std::io::Write as _;
use std::sync::Mutex;

use clap::ValueEnum;
use pmt_agent::turn::EventSink;
use pmt_agent::turn::model::BoxFuture;
use pmt_core::secret::Redactor;
use pmt_obs::trajectory::writer::Emit;
use pmt_obs::trajectory::{Line, timestamp, version};
use serde::Serialize;
use serde_json::Value;

/// `--output-format` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// The answer as text (the default).
    Text,
    /// One JSON object at the end (as `--json`).
    Json,
    /// One JSON object per event as it happens, then a `result` line.
    StreamJson,
}

/// Writes `value` as one line to stdout and flushes it, so a reader sees
/// each event as soon as it happens (fields in declaration order).
pub fn write_line<T: Serialize + ?Sized>(value: &T) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", serde_json::to_string(value).unwrap_or_default());
    let _ = out.flush();
}

/// The closing `result` line, `type` first like the event lines.
#[derive(Debug, Clone, Serialize)]
pub struct ResultLine {
    #[serde(rename = "type")]
    kind: &'static str,
    session_id: String,
    answer: Option<String>,
    usage: Value,
    exit_reason: String,
}

/// The closing `result` line.
#[must_use]
pub fn result_line(
    session_id: &str,
    answer: Option<&str>,
    usage: &Value,
    exit_reason: &str,
) -> ResultLine {
    ResultLine {
        kind: "result",
        session_id: session_id.to_owned(),
        answer: answer.map(str::to_owned),
        usage: usage.clone(),
        exit_reason: exit_reason.to_owned(),
    }
}

/// The event sink of a streamed one-shot run: every event of the session
/// as a stdout line.
pub struct Stream {
    session_id: String,
    root_session_id: String,
    redactor: Option<Redactor>,
    seq: Mutex<u64>,
}

impl Stream {
    /// The stream of session `session_id` (lineage root `root`).
    #[must_use]
    pub fn new(session_id: String, root: String, redactor: Option<Redactor>) -> Self {
        Self {
            session_id,
            root_session_id: root,
            redactor,
            seq: Mutex::new(0),
        }
    }

    fn line(&self, emit: &Emit) -> Line {
        let payload = match &self.redactor {
            Some(redactor) => redactor.redact_json(emit.event.payload()),
            None => emit.event.payload(),
        };
        let mut seq = self
            .seq
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *seq += 1;
        Line {
            v: version(),
            ts: timestamp(emit.ts),
            seq: *seq,
            session_id: self.session_id.clone(),
            root_session_id: self.root_session_id.clone(),
            turn_id: emit.turn_id.clone(),
            span_id: emit.span_id.clone(),
            parent_span_id: emit.parent_span_id.clone(),
            kind: emit.event.kind().to_owned(),
            payload,
        }
    }
}

impl EventSink for Stream {
    fn emit(&self, emit: Emit) -> BoxFuture<'_, Result<(), pmt_core::Error>> {
        write_line(&self.line(&emit));
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use pmt_obs::trajectory::events::Event;
    use pmt_obs::trajectory::payload::MessageText;

    use super::*;

    #[test]
    fn a_streamed_event_has_the_trajectory_line_shape() {
        let stream = Stream::new("ses_a".into(), "ses_a".into(), None);
        let emit = Emit::now(Event::AssistantMessage(MessageText {
            message_id: None,
            text: "hi".into(),
        }))
        .in_turn("trn_1", None);
        let first = serde_json::to_value(stream.line(&emit)).unwrap();
        let second = stream.line(&emit);
        assert_eq!(first["type"], "assistant.message");
        assert_eq!(first["payload"]["text"], "hi");
        assert_eq!(first["turn_id"], "trn_1");
        assert_eq!(first["session_id"], "ses_a");
        assert_eq!((first["seq"].as_u64(), second.seq), (Some(1), 2));
        let text =
            serde_json::to_string(&result_line("ses_a", Some("hi"), &Value::Null, "completed"))
                .unwrap();
        assert!(text.starts_with("{\"type\":\"result\""), "{text}");
    }
}
