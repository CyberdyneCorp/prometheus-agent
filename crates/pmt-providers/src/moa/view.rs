//! What each slot of a preset is sent (spec: mixture-of-agents, MoA call
//! procedure; Preset validation): the references get a deterministic
//! view of the conversation with only user and assistant text (no system
//! prompt, no tools, no tool calls or results); every slot gets its own
//! reasoning effort and, only when its temperature key is set, a
//! temperature.

use std::sync::Arc;

use pmt_config::schema::{MoaSlot, ReasoningLevel};
use serde_json::{Map, Value, json};

use crate::descriptor::BundledWire;
use crate::request::{EXTRA_BODY, Message, Part, Request, Role};
use crate::resolve::Route;

/// The references' view of `base`: user and assistant text only, adjacent
/// turns of one author merged, ending at the last user turn.
#[must_use]
pub fn reference_view(base: &Request) -> Request {
    let mut messages: Vec<Message> = Vec::new();
    for message in &base.messages {
        if message.role == Role::Tool {
            continue;
        }
        let parts: Vec<Part> = message
            .parts
            .iter()
            .filter(|p| matches!(p, Part::Text { text } if !text.trim().is_empty()))
            .cloned()
            .collect();
        if parts.is_empty() {
            continue;
        }
        match messages.last_mut() {
            Some(last) if last.role == message.role => last.parts.extend(parts),
            _ => messages.push(Message {
                role: message.role,
                parts,
                cache_breakpoint: false,
            }),
        }
    }
    while messages.last().is_some_and(|m| m.role == Role::Assistant) {
        messages.pop();
    }
    Request {
        system: Vec::new(),
        messages,
        tools: Vec::new(),
        server_compaction: None,
        speed: None,
        ..base.clone()
    }
}

/// `request` as `slot` on `route` receives it: the slot's effort (else
/// the session's), and `temperature` only when the preset sets it.
#[must_use]
pub fn for_slot(
    request: &Request,
    slot: &MoaSlot,
    route: &Route,
    session_effort: Option<ReasoningLevel>,
    temperature: Option<f64>,
) -> Request {
    let mut out = request.clone();
    out.model.clone_from(&route.model);
    out.reasoning = slot.reasoning_effort.or(session_effort);
    if let Some(t) = temperature {
        let mut overrides = request
            .body_overrides
            .as_deref()
            .cloned()
            .unwrap_or_default();
        merge(&mut overrides, temperature_fields(route, t));
        out.body_overrides = Some(Arc::new(overrides));
    }
    out
}

/// Where `route`'s wire takes a temperature.
fn temperature_fields(route: &Route, t: f64) -> Map<String, Value> {
    let value = match route.bundled {
        Some(BundledWire::GeminiNative) => {
            json!({ EXTRA_BODY: { "generationConfig": { "temperature": t } } })
        }
        Some(BundledWire::BedrockConverse) => {
            json!({ EXTRA_BODY: { "inferenceConfig": { "temperature": t } } })
        }
        None => json!({ "temperature": t }),
    };
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// Adds `more` to `overrides`, merging the `extra_body` tables.
fn merge(overrides: &mut Map<String, Value>, more: Map<String, Value>) {
    for (key, value) in more {
        match (overrides.get_mut(&key), value) {
            (Some(Value::Object(existing)), Value::Object(added)) if key == EXTRA_BODY => {
                existing.extend(added);
            }
            (_, value) => {
                overrides.insert(key, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{SystemBlock, Tool};

    #[test]
    fn the_view_keeps_only_user_and_assistant_text() {
        let base = Request {
            system: vec![SystemBlock {
                text: "sys".into(),
                cache_breakpoint: true,
            }],
            tools: vec![Tool {
                name: "read_file".into(),
                description: String::new(),
                parameters: json!({}),
            }],
            messages: vec![
                Message::text(Role::User, "fix it"),
                Message {
                    role: Role::Assistant,
                    parts: vec![
                        Part::text("looking"),
                        Part::ToolCall {
                            id: "c1".into(),
                            name: "read_file".into(),
                            arguments: "{}".into(),
                        },
                    ],
                    cache_breakpoint: true,
                },
                Message {
                    role: Role::Tool,
                    parts: vec![Part::ToolResult {
                        call_id: "c1".into(),
                        content: "secret file".into(),
                        is_error: false,
                    }],
                    cache_breakpoint: false,
                },
                Message::text(Role::Assistant, "found it"),
            ],
            ..Request::default()
        };
        let view = reference_view(&base);
        assert!(view.system.is_empty() && view.tools.is_empty());
        assert_eq!(view.messages, [Message::text(Role::User, "fix it")]);
        assert_eq!(reference_view(&base), view, "deterministic");
    }
}
