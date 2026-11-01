//! The stored transcript as request messages, and the rows a turn writes.
//!
//! Each row stores its exact wire content, so replaying the transcript
//! rebuilds earlier requests byte-for-byte (spec: agent-turn-loop,
//! Request-time additions never touch history).

use pmt_providers::request::{Message, Part, Role as WireRole};
use pmt_store::{Message as Row, NewMessage, Role};
use pmt_tools::{Status, ToolResult};
use serde_json::{Value, json};

use super::model::{ModelReply, RequestedCall};

/// Metadata key marking rows Prometheus wrote itself (notices, boundary
/// rows), not the user or the model.
pub const AUTHORED: &str = "prometheus_authored";

/// The request message of one stored row (none for system rows); see
/// [`super::repair::wire`] for the whole transcript.
pub(super) fn message(row: &Row) -> Option<Message> {
    let text = row.content.clone().unwrap_or_default();
    let (role, parts) = match row.role {
        Role::User => (WireRole::User, vec![Part::text(text)]),
        Role::Assistant => (
            WireRole::Assistant,
            assistant_parts(text, row.tool_calls.as_ref()),
        ),
        Role::Tool => (
            WireRole::Tool,
            vec![Part::ToolResult {
                call_id: row.tool_call_id.clone().unwrap_or_default(),
                content: text,
                is_error: row
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("is_error"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }],
        ),
        Role::System => return None,
    };
    Some(Message {
        role,
        parts,
        cache_breakpoint: false,
    })
}

fn assistant_parts(text: String, calls: Option<&Value>) -> Vec<Part> {
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(Part::text(text));
    }
    for call in calls.and_then(Value::as_array).into_iter().flatten() {
        let field = |key: &str| call.get(key).and_then(Value::as_str).unwrap_or_default();
        parts.push(Part::ToolCall {
            id: field("id").to_owned(),
            name: field("name").to_owned(),
            arguments: field("arguments").to_owned(),
        });
    }
    parts
}

/// Metadata key of the images a user row carries.
pub const IMAGES: &str = "images";

/// An image attached to a user message, kept in the session blob store
/// (spec: cli, Image attachments). How it is delivered is decided once,
/// when the message is composed, and kept with the message: pixels, or
/// the `vision` route's description when the main model cannot see.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ImageRef {
    /// Blob store reference (`blob:sha256:<hex>`).
    pub blob: String,
    /// MIME type (`image/png`).
    pub media_type: String,
    /// The file name shown in notes.
    pub name: String,
    /// The description sent instead of the pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The images stored on `row` (none for rows without attachments).
#[must_use]
pub fn images_of(row: &Row) -> Vec<ImageRef> {
    row.metadata
        .as_ref()
        .and_then(|m| m.get(IMAGES))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

/// The user row: the user's text plus request-time additions (memory
/// prefetch, plugin context, image descriptions), stored as sent, and the
/// images sent as pixels.
#[must_use]
pub fn user_row(text: &str, additions: &[String], images: &[ImageRef]) -> NewMessage {
    if additions.is_empty() && images.is_empty() {
        return NewMessage::new(Role::User, text);
    }
    let mut wire = text.to_owned();
    for addition in additions {
        wire.push_str("\n\n");
        wire.push_str(addition);
    }
    let mut row = NewMessage::new(Role::User, wire);
    let mut metadata = json!({ "user_text": text });
    if !images.is_empty() {
        metadata[IMAGES] = json!(images);
    }
    row.metadata = Some(metadata);
    row
}

/// A user-role notice Prometheus adds (the budget handoff request).
#[must_use]
pub fn notice_row(text: &str, kind: &str) -> NewMessage {
    let mut row = NewMessage::new(Role::User, text);
    row.metadata = Some(json!({ AUTHORED: true, "kind": kind }));
    row
}

/// The assistant row of a reply.
#[must_use]
pub fn assistant_row(reply: &ModelReply) -> NewMessage {
    let mut row = NewMessage::new(Role::Assistant, reply.text.clone());
    if !reply.tool_calls.is_empty() {
        row.tool_calls = Some(Value::Array(
            reply.tool_calls.iter().map(call_json).collect(),
        ));
    }
    row.reasoning = reply.reasoning.as_ref().map(|r| json!([{ "text": r }]));
    row
}

fn call_json(call: &RequestedCall) -> Value {
    json!({ "id": call.id, "name": call.name, "arguments": call.arguments })
}

/// The tool row answering `call`.
#[must_use]
pub fn tool_row(call: &RequestedCall, result: &ToolResult) -> NewMessage {
    let mut row = NewMessage::new(Role::Tool, result.text_content());
    row.tool_call_id = Some(call.id.clone());
    row.tool_name = Some(call.name.clone());
    row.metadata = Some(json!({
        "status": result.status.as_str(),
        "is_error": !matches!(result.status, Status::Ok | Status::Degraded),
    }));
    row
}
