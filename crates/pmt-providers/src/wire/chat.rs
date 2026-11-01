//! OpenAI Chat Completions (`/chat/completions`, streamed).

use serde_json::{Map, Value, json};

use super::{EncodeOptions, count, image_url, json, system_text, truncated};
use crate::classify::{ProviderError, body_message, classify_stream_error};
use crate::event::{Normalizer, StopReason, UsageReport};
use crate::request::{Message, Part, Request, Role};
use crate::sse::SseEvent;

pub(super) fn encode(request: &Request, options: &EncodeOptions<'_>) -> Value {
    let mut messages = Vec::new();
    if let Some(system) = system_text(request) {
        messages.push(json!({"role": "system", "content": system}));
    }
    for message in &request.messages {
        encode_message(message, &mut messages);
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert("messages".into(), Value::Array(messages));
    if !request.tools.is_empty() {
        let tools = request
            .tools
            .iter()
            .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.parameters}}))
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(max) = request.max_output_tokens {
        body.insert("max_tokens".into(), json!(max));
    }
    if let Some(level) = request.reasoning {
        body.insert("reasoning_effort".into(), json!(level.as_str()));
    }
    body.insert("stream".into(), json!(true));
    body.insert("stream_options".into(), json!({"include_usage": true}));
    if let Some(key) = options.cache_key {
        body.insert("prompt_cache_key".into(), json!(key));
    }
    Value::Object(body)
}

fn encode_message(message: &Message, out: &mut Vec<Value>) {
    let mut content = Vec::new();
    let mut tool_calls = Vec::new();
    for part in &message.parts {
        match part {
            Part::Text { text } => content.push(json!({"type": "text", "text": text})),
            Part::Image { media_type, data, is_url } => content.push(
                json!({"type": "image_url", "image_url": {"url": image_url(media_type, data, *is_url)}}),
            ),
            Part::ToolCall { id, name, arguments } => tool_calls.push(
                json!({"id": id, "type": "function", "function": {"name": name, "arguments": arguments}}),
            ),
            Part::ToolResult { call_id, content: text, .. } => {
                out.push(json!({"role": "tool", "tool_call_id": call_id, "content": text}));
            }
        }
    }
    if content.is_empty() && tool_calls.is_empty() {
        return;
    }
    let role = match message.role {
        Role::Assistant => "assistant",
        Role::User | Role::Tool => "user",
    };
    let mut entry = Map::new();
    entry.insert("role".into(), json!(role));
    entry.insert("content".into(), simple_content(content));
    if !tool_calls.is_empty() {
        entry.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    out.push(Value::Object(entry));
}

/// A lone text part is sent as a plain string (what every compatible
/// server accepts); anything else as a part array.
fn simple_content(mut parts: Vec<Value>) -> Value {
    match parts.as_slice() {
        [] => Value::Null,
        [only] if only["type"] == "text" => parts.swap_remove(0)["text"].take(),
        _ => Value::Array(parts),
    }
}

#[derive(Debug, Default)]
pub(super) struct Decoder {
    reason: Option<StopReason>,
}

impl super::Decode for Decoder {
    fn event(&mut self, event: &SseEvent, out: &mut Normalizer) -> Result<(), ProviderError> {
        if event.data.trim() == "[DONE]" {
            out.finish(self.reason);
            return Ok(());
        }
        let chunk = json(event)?;
        if let Some(error) = chunk.get("error") {
            let class = classify_stream_error(&error.to_string());
            return Err(ProviderError::new(class, body_message(&event.data)));
        }
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            out.usage(usage_report(usage));
        }
        let Some(choice) = chunk.pointer("/choices/0") else {
            return Ok(());
        };
        let delta = &choice["delta"];
        if let Some(text) = delta["content"].as_str() {
            out.text(text);
        }
        for key in ["reasoning_content", "reasoning"] {
            if let Some(text) = delta[key].as_str() {
                out.reasoning(text);
            }
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            tool_call(call, out);
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.reason = Some(stop_reason(reason));
        }
        Ok(())
    }

    fn end(&mut self, out: &mut Normalizer) -> Result<(), ProviderError> {
        // Some servers close the stream without `[DONE]` after the last
        // chunk; a finish reason means the response was complete.
        if self.reason.is_some() {
            out.finish(self.reason);
            Ok(())
        } else {
            Err(truncated())
        }
    }
}

fn tool_call(call: &Value, out: &mut Normalizer) {
    let index = call["index"]
        .as_u64()
        .and_then(|i| usize::try_from(i).ok())
        .unwrap_or(0);
    let name = call.pointer("/function/name").and_then(Value::as_str);
    if let (Some(id), Some(name)) = (call["id"].as_str(), name) {
        out.tool_start(index, id, name);
    }
    if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
        out.tool_arguments(index, arguments);
    }
}

fn stop_reason(reason: &str) -> StopReason {
    match reason {
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::ContentFilter,
        _ => StopReason::EndTurn,
    }
}

/// `prompt_tokens` includes cached input; the ledger's input does not.
fn usage_report(usage: &Value) -> UsageReport {
    let prompt = count(usage, "/prompt_tokens").unwrap_or(0);
    let cached = count(usage, "/prompt_tokens_details/cached_tokens")
        .or_else(|| count(usage, "/prompt_cache_hit_tokens"))
        .unwrap_or(0);
    UsageReport {
        input: Some(prompt.saturating_sub(cached)),
        output: count(usage, "/completion_tokens"),
        cache_read: Some(cached),
        cache_write: count(usage, "/prompt_tokens_details/cache_write_tokens"),
        reasoning: count(usage, "/completion_tokens_details/reasoning_tokens"),
        cost_usd: usage.get("cost").and_then(Value::as_f64),
    }
}
