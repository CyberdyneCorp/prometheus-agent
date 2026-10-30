//! OpenAI Responses (`/responses`, streamed).

use serde_json::{Map, Value, json};

use super::{EncodeOptions, count, image_url, json, system_text, truncated};
use crate::classify::{ProviderError, body_message, classify_stream_error};
use crate::event::{Normalizer, StopReason, UsageReport};
use crate::request::{Message, Part, Request, Role};
use crate::sse::SseEvent;

pub(super) fn encode(request: &Request, options: &EncodeOptions<'_>) -> Value {
    let mut input = Vec::new();
    for message in &request.messages {
        encode_message(message, &mut input);
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    if let Some(system) = system_text(request) {
        body.insert("instructions".into(), json!(system));
    }
    body.insert("input".into(), Value::Array(input));
    if !request.tools.is_empty() {
        let tools = request
            .tools
            .iter()
            .map(|t| json!({"type": "function", "name": t.name, "description": t.description, "parameters": t.parameters}))
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(max) = request.max_output_tokens {
        body.insert("max_output_tokens".into(), json!(max));
    }
    body.insert("stream".into(), json!(true));
    body.insert("store".into(), json!(false));
    if let Some(key) = options.cache_key {
        body.insert("prompt_cache_key".into(), json!(key));
    }
    Value::Object(body)
}

fn encode_message(message: &Message, out: &mut Vec<Value>) {
    let assistant = message.role == Role::Assistant;
    let mut content = Vec::new();
    for part in &message.parts {
        match part {
            Part::Text { text } if assistant => content.push(json!({"type": "output_text", "text": text})),
            Part::Text { text } => content.push(json!({"type": "input_text", "text": text})),
            Part::Image { media_type, data, is_url } => content.push(
                json!({"type": "input_image", "image_url": image_url(media_type, data, *is_url)}),
            ),
            Part::ToolCall { id, name, arguments } => out.push(
                json!({"type": "function_call", "call_id": id, "name": name, "arguments": arguments}),
            ),
            Part::ToolResult { call_id, content: text, .. } => out.push(
                json!({"type": "function_call_output", "call_id": call_id, "output": text}),
            ),
        }
    }
    if !content.is_empty() {
        let role = if assistant { "assistant" } else { "user" };
        out.push(json!({"type": "message", "role": role, "content": content}));
    }
}

#[derive(Debug, Default)]
pub(super) struct Decoder;

impl super::Decode for Decoder {
    fn event(&mut self, event: &SseEvent, out: &mut Normalizer) -> Result<(), ProviderError> {
        let data = json(event)?;
        let kind = event
            .event
            .as_deref()
            .or_else(|| data["type"].as_str())
            .unwrap_or_default();
        let index = data["output_index"]
            .as_u64()
            .and_then(|i| usize::try_from(i).ok())
            .unwrap_or(0);
        match kind {
            "response.output_text.delta" => out.text(data["delta"].as_str().unwrap_or_default()),
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                out.reasoning(data["delta"].as_str().unwrap_or_default());
            }
            "response.output_item.added" if data["item"]["type"] == "function_call" => {
                let item = &data["item"];
                let id = item["call_id"].as_str().unwrap_or_default();
                out.tool_start(index, id, item["name"].as_str().unwrap_or_default());
            }
            "response.function_call_arguments.delta" => {
                out.tool_arguments(index, data["delta"].as_str().unwrap_or_default());
            }
            "response.output_item.done" => out.tool_end(index),
            "response.completed" => finish(&data["response"], None, out),
            "response.incomplete" => {
                let reason = match data
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str)
                {
                    Some("content_filter") => StopReason::ContentFilter,
                    _ => StopReason::MaxTokens,
                };
                finish(&data["response"], Some(reason), out);
            }
            "response.failed" | "error" => return Err(failure(&data)),
            _ => {}
        }
        Ok(())
    }

    fn end(&mut self, _out: &mut Normalizer) -> Result<(), ProviderError> {
        Err(truncated())
    }
}

fn finish(response: &Value, reason: Option<StopReason>, out: &mut Normalizer) {
    let usage = &response["usage"];
    if usage.is_object() {
        let input = count(usage, "/input_tokens").unwrap_or(0);
        let cached = count(usage, "/input_tokens_details/cached_tokens").unwrap_or(0);
        out.usage(UsageReport {
            input: Some(input.saturating_sub(cached)),
            output: count(usage, "/output_tokens"),
            cache_read: Some(cached),
            cache_write: None,
            reasoning: count(usage, "/output_tokens_details/reasoning_tokens"),
            cost_usd: usage.get("cost").and_then(Value::as_f64),
        });
    }
    out.finish(reason);
}

fn failure(data: &Value) -> ProviderError {
    let error = data.pointer("/response/error").unwrap_or(data);
    let text = error.to_string();
    ProviderError::new(
        classify_stream_error(&text),
        body_message(&json!({"error": error}).to_string()),
    )
}
