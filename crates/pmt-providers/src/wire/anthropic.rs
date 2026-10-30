//! Anthropic Messages (`/v1/messages`, streamed) with explicit
//! `cache_control` breakpoints.

use serde_json::{Map, Value, json};

use super::{count, json, truncated};
use crate::classify::{ProviderError, body_message, classify_stream_error};
use crate::event::{Normalizer, StopReason, UsageReport};
use crate::request::{CacheTtl, Message, Part, Request, Role};
use crate::sse::SseEvent;

/// The API version every request names.
pub(crate) const API_VERSION: &str = "2023-06-01";
/// Anthropic accepts at most four cache breakpoints per request.
const MAX_BREAKPOINTS: usize = 4;
/// `max_tokens` is required; used when the request sets no cap.
const DEFAULT_MAX_TOKENS: u64 = 8_192;

pub(super) fn encode(request: &Request) -> Value {
    let mut budget = Breakpoints::new(request);
    let system: Vec<Value> = request
        .system
        .iter()
        .map(|b| {
            marked(
                json!({"type": "text", "text": b.text}),
                budget.system(b.cache_breakpoint),
            )
        })
        .collect();
    let last_tool = request.tools.len().saturating_sub(1);
    let tools: Vec<Value> = request
        .tools
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let tool =
                json!({"name": t.name, "description": t.description, "input_schema": t.parameters});
            marked(
                tool,
                budget.tools(i == last_tool && request.cache.tools_breakpoint),
            )
        })
        .collect();
    let messages: Vec<Value> = request
        .messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| encode_message(m, budget.message(i)))
        .collect();
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert(
        "max_tokens".into(),
        json!(request.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
    );
    if !system.is_empty() {
        body.insert("system".into(), Value::Array(system));
    }
    body.insert("messages".into(), Value::Array(messages));
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    body.insert("stream".into(), json!(true));
    Value::Object(body)
}

/// Which requested breakpoints get a marker: system and tool breakpoints
/// first, then the latest message breakpoints, at most four in all; none
/// when caching is off.
struct Breakpoints {
    marker: Option<Value>,
    left: usize,
    messages: Vec<bool>,
}

impl Breakpoints {
    fn new(request: &Request) -> Self {
        let marker = match request.cache.ttl {
            CacheTtl::Off => None,
            CacheTtl::FiveMinutes => Some(json!({"type": "ephemeral"})),
            CacheTtl::OneHour => Some(json!({"type": "ephemeral", "ttl": "1h"})),
        };
        let fixed = request.system.iter().filter(|b| b.cache_breakpoint).count()
            + usize::from(request.cache.tools_breakpoint && !request.tools.is_empty());
        let mut left = MAX_BREAKPOINTS.saturating_sub(fixed);
        let mut messages = vec![false; request.messages.len()];
        for (i, m) in request.messages.iter().enumerate().rev() {
            if m.cache_breakpoint && left > 0 {
                messages[i] = true;
                left -= 1;
            }
        }
        Self {
            marker,
            left: MAX_BREAKPOINTS,
            messages,
        }
    }

    fn take(&mut self, wanted: bool) -> Option<Value> {
        if !wanted || self.left == 0 {
            return None;
        }
        self.left -= 1;
        self.marker.clone()
    }

    fn system(&mut self, wanted: bool) -> Option<Value> {
        self.take(wanted)
    }

    fn tools(&mut self, wanted: bool) -> Option<Value> {
        self.take(wanted)
    }

    fn message(&mut self, index: usize) -> Option<Value> {
        let wanted = self.messages[index];
        self.take(wanted)
    }
}

fn marked(mut block: Value, marker: Option<Value>) -> Value {
    if let Some(marker) = marker {
        block["cache_control"] = marker;
    }
    block
}

fn encode_message(message: &Message, marker: Option<Value>) -> Option<Value> {
    let mut content: Vec<Value> = message.parts.iter().map(encode_part).collect();
    if let Some(last) = content.last_mut() {
        *last = marked(last.take(), marker);
    } else {
        return None;
    }
    let role = if message.role == Role::Assistant {
        "assistant"
    } else {
        "user"
    };
    Some(json!({"role": role, "content": content}))
}

fn encode_part(part: &Part) -> Value {
    match part {
        Part::Text { text } => json!({"type": "text", "text": text}),
        Part::Image {
            media_type,
            data,
            is_url: true,
        } => {
            json!({"type": "image", "source": {"type": "url", "url": data, "media_type": media_type}})
        }
        Part::Image {
            media_type, data, ..
        } => {
            json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}})
        }
        Part::ToolCall {
            id,
            name,
            arguments,
        } => {
            let input: Value = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
            json!({"type": "tool_use", "id": id, "name": name, "input": input})
        }
        Part::ToolResult {
            call_id,
            content,
            is_error,
        } => {
            json!({"type": "tool_result", "tool_use_id": call_id, "content": content, "is_error": is_error})
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct Decoder {
    reason: Option<StopReason>,
}

impl super::Decode for Decoder {
    fn event(&mut self, event: &SseEvent, out: &mut Normalizer) -> Result<(), ProviderError> {
        let data = json(event)?;
        let kind = event
            .event
            .as_deref()
            .or_else(|| data["type"].as_str())
            .unwrap_or_default();
        let index = data["index"]
            .as_u64()
            .and_then(|i| usize::try_from(i).ok())
            .unwrap_or(0);
        match kind {
            "message_start" => out.usage(usage_report(&data["message"]["usage"])),
            "content_block_start" if data["content_block"]["type"] == "tool_use" => {
                let block = &data["content_block"];
                out.tool_start(
                    index,
                    block["id"].as_str().unwrap_or_default(),
                    block["name"].as_str().unwrap_or_default(),
                );
            }
            "content_block_delta" => delta(&data["delta"], index, out),
            "content_block_stop" => out.tool_end(index),
            "message_delta" => {
                if let Some(reason) = data.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.reason = Some(stop_reason(reason));
                }
                out.usage(usage_report(&data["usage"]));
            }
            "message_stop" => out.finish(self.reason),
            "error" => {
                let class = classify_stream_error(&data["error"].to_string());
                return Err(ProviderError::new(class, body_message(&event.data)));
            }
            _ => {}
        }
        Ok(())
    }

    fn end(&mut self, _out: &mut Normalizer) -> Result<(), ProviderError> {
        Err(truncated())
    }
}

fn delta(delta: &Value, index: usize, out: &mut Normalizer) {
    match delta["type"].as_str() {
        Some("text_delta") => out.text(delta["text"].as_str().unwrap_or_default()),
        Some("thinking_delta") => out.reasoning(delta["thinking"].as_str().unwrap_or_default()),
        Some("input_json_delta") => {
            out.tool_arguments(index, delta["partial_json"].as_str().unwrap_or_default());
        }
        _ => {}
    }
}

fn stop_reason(reason: &str) -> StopReason {
    match reason {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "refusal" => StopReason::ContentFilter,
        _ => StopReason::EndTurn,
    }
}

/// Anthropic's `input_tokens` already excludes cache reads and writes.
fn usage_report(usage: &Value) -> UsageReport {
    UsageReport {
        input: count(usage, "/input_tokens"),
        output: count(usage, "/output_tokens"),
        cache_read: count(usage, "/cache_read_input_tokens"),
        cache_write: count(usage, "/cache_creation_input_tokens"),
        reasoning: None,
        cost_usd: None,
    }
}
