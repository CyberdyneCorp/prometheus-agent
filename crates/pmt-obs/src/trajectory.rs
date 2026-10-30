//! The trajectory line shape (spec: observability, One trajectory grammar
//! for every surface): exactly the top-level keys `v`, `ts`, `seq`,
//! `session_id`, `root_session_id`, `turn_id`, `span_id`,
//! `parent_span_id`, `type` and `payload`.

use serde::Serialize;
use serde_json::Value;

/// One JSONL line of a trajectory.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Line {
    /// Grammar version, `<major>.<minor>`.
    pub v: String,
    /// RFC 3339 UTC with microseconds.
    pub ts: String,
    /// Per-session sequence number from 1.
    pub seq: u64,
    /// The session.
    pub session_id: String,
    /// Its lineage root.
    pub root_session_id: String,
    /// The turn, `null` outside one.
    pub turn_id: Option<String>,
    /// This event's span.
    pub span_id: String,
    /// The enclosing span.
    pub parent_span_id: Option<String>,
    /// Event type, for example `user.message`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Typed payload of the event.
    pub payload: Value,
}

/// The `v` value this build writes.
#[must_use]
pub fn version() -> String {
    format!("{}.0", crate::GRAMMAR_VERSION)
}

/// `ts` formatting: RFC 3339 UTC with microseconds.
#[must_use]
pub fn timestamp(ts: jiff::Timestamp) -> String {
    ts.strftime("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::{Line, timestamp, version};

    #[test]
    fn lines_have_exactly_the_grammar_keys() {
        let line = Line {
            v: version(),
            ts: timestamp(jiff::Timestamp::UNIX_EPOCH),
            seq: 1,
            session_id: "ses_A".to_owned(),
            root_session_id: "ses_A".to_owned(),
            turn_id: None,
            span_id: "span".to_owned(),
            parent_span_id: None,
            kind: "user.message",
            payload: serde_json::json!({"text": "hi"}),
        };
        let value = serde_json::to_value(&line).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "parent_span_id",
                "payload",
                "root_session_id",
                "seq",
                "session_id",
                "span_id",
                "ts",
                "turn_id",
                "type",
                "v"
            ]
        );
        assert_eq!(value["ts"], "1970-01-01T00:00:00.000000Z");
        assert_eq!(value["v"], "1.0");
    }
}
