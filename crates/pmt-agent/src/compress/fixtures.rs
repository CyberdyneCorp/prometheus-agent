//! Stored rows for the compression unit tests.

use jiff::Timestamp;
use pmt_core::id::{MessageId, SessionId};
use pmt_store::{Message as Row, Role};
use serde_json::{Value, json};

/// A stored row of `role` with `text`.
pub(crate) fn row(role: Role, text: &str) -> Row {
    Row {
        id: MessageId::new(),
        session_id: SessionId::new(),
        seq: 0,
        role,
        content: Some(text.to_owned()),
        tool_calls: None,
        reasoning: None,
        provider_payload: None,
        tool_call_id: None,
        tool_name: None,
        metadata: None,
        content_hash: String::new(),
        created_at: Timestamp::UNIX_EPOCH,
        active: true,
        compacted: false,
        compaction_epoch: None,
        content_ref: None,
    }
}

/// Row builders.
pub(crate) trait Build {
    /// An assistant row calling `lookup` with id `id`.
    fn with_call(self, id: &str) -> Self;
    /// An assistant row calling `name(arguments)` with id `id`.
    fn calling(self, id: &str, name: &str, arguments: &str) -> Self;
    /// A tool row answering `id` (tool `lookup`).
    fn answering(self, id: &str) -> Self;
    /// A tool row of `name` answering `id`.
    fn answering_as(self, id: &str, name: &str) -> Self;
    /// Adds `key = value` to the metadata.
    fn meta(self, key: &str, value: Value) -> Self;
}

impl Build for Row {
    fn with_call(self, id: &str) -> Self {
        self.calling(id, "lookup", r#"{"q":"x"}"#)
    }

    fn calling(mut self, id: &str, name: &str, arguments: &str) -> Self {
        let mut calls = self
            .tool_calls
            .take()
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        calls.push(json!({"id": id, "name": name, "arguments": arguments}));
        self.tool_calls = Some(Value::Array(calls));
        self
    }

    fn answering(self, id: &str) -> Self {
        self.answering_as(id, "lookup")
    }

    fn answering_as(mut self, id: &str, name: &str) -> Self {
        self.tool_call_id = Some(id.to_owned());
        self.tool_name = Some(name.to_owned());
        self
    }

    fn meta(mut self, key: &str, value: Value) -> Self {
        let mut map = self
            .metadata
            .take()
            .and_then(|m| m.as_object().cloned())
            .unwrap_or_default();
        map.insert(key.to_owned(), value);
        self.metadata = Some(Value::Object(map));
        self
    }
}
