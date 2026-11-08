//! AWS Bedrock `ConverseStream` (`bedrock_converse`, spec:
//! model-providers, Gemini native and AWS Bedrock providers). The model
//! id is part of the URL; the response is an AWS event stream whose
//! frames [`crate::aws::eventstream`] turns into events named by their
//! `:event-type` (or `exception.<type>`).

use pmt_config::schema::ReasoningLevel;
use serde_json::{Map, Value, json};

use super::{count, json, truncated};
use crate::classify::{ErrorClass, ProviderError, body_message};
use crate::event::{Normalizer, StopReason, UsageReport};
use crate::request::{Message, Part, Request, Role};
use crate::sse::SseEvent;

/// Converse requires `maxTokens` when thinking is on.
const DEFAULT_MAX_TOKENS: u64 = 8_192;

pub(super) fn encode(request: &Request) -> Value {
    let mut body = Map::new();
    let system: Vec<Value> = request
        .system
        .iter()
        .filter(|b| !b.text.is_empty())
        .map(|b| json!({"text": b.text}))
        .collect();
    if !system.is_empty() {
        body.insert("system".into(), Value::Array(system));
    }
    body.insert("messages".into(), Value::Array(messages(request)));
    let budget = request
        .reasoning
        .filter(|_| request.model.contains("anthropic.claude"))
        .and_then(claude_budget);
    let max_tokens = request
        .max_output_tokens
        .or(budget.map(|_| DEFAULT_MAX_TOKENS));
    if let Some(max) = max_tokens {
        // Thinking tokens count against the cap, as on Anthropic's API.
        let max = max + budget.unwrap_or(0);
        body.insert("inferenceConfig".into(), json!({"maxTokens": max}));
    }
    if let Some(budget) = budget {
        body.insert(
            "additionalModelRequestFields".into(),
            json!({"thinking": {"type": "enabled", "budget_tokens": budget}}),
        );
    }
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                json!({"toolSpec": {"name": t.name, "description": t.description, "inputSchema": {"json": t.parameters}}})
            })
            .collect();
        body.insert("toolConfig".into(), json!({ "tools": tools }));
    }
    Value::Object(body)
}

/// Claude's extended-thinking budget on Bedrock (as on Anthropic's API).
const fn claude_budget(level: ReasoningLevel) -> Option<u64> {
    match level {
        ReasoningLevel::None => None,
        ReasoningLevel::Minimal => Some(1_024),
        ReasoningLevel::Low => Some(4_096),
        ReasoningLevel::Medium => Some(8_192),
        ReasoningLevel::High => Some(16_384),
        ReasoningLevel::Xhigh => Some(32_768),
        ReasoningLevel::Max | ReasoningLevel::Ultra => Some(64_000),
    }
}

/// Converse alternates `user` and `assistant`: tool results are user
/// turns and adjacent turns of one role are merged.
fn messages(request: &Request) -> Vec<Value> {
    let mut out: Vec<(&'static str, Vec<Value>)> = Vec::new();
    for message in &request.messages {
        let role = if message.role == Role::Assistant {
            "assistant"
        } else {
            "user"
        };
        let content = content(message);
        if content.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some((last, existing)) if *last == role => existing.extend(content),
            _ => out.push((role, content)),
        }
    }
    out.into_iter()
        .map(|(role, content)| json!({"role": role, "content": content}))
        .collect()
}

fn content(message: &Message) -> Vec<Value> {
    message
        .parts
        .iter()
        .filter_map(|part| match part {
            // Converse refuses empty text blocks.
            Part::Text { text } if text.is_empty() => None,
            Part::Text { text } => Some(json!({"text": text})),
            // Converse takes image bytes only; a URL stays a reference.
            Part::Image {
                data, is_url: true, ..
            } => Some(json!({"text": format!("[image: {data}]")})),
            Part::Image {
                media_type, data, ..
            } => {
                let format = media_type.strip_prefix("image/").unwrap_or(media_type);
                let format = if format == "jpg" { "jpeg" } else { format };
                Some(json!({"image": {"format": format, "source": {"bytes": data}}}))
            }
            Part::ToolCall {
                id,
                name,
                arguments,
            } => {
                let input: Value = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
                Some(json!({"toolUse": {"toolUseId": id, "name": name, "input": input}}))
            }
            Part::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                let mut result = json!({"toolUseId": call_id, "content": [{"text": content}]});
                if *is_error {
                    result["status"] = json!("error");
                }
                Some(json!({ "toolResult": result }))
            }
        })
        .collect()
}

#[derive(Debug, Default)]
pub(super) struct Decoder {
    reason: Option<StopReason>,
}

impl super::Decode for Decoder {
    fn event(&mut self, event: &SseEvent, out: &mut Normalizer) -> Result<(), ProviderError> {
        let kind = event.event.as_deref().unwrap_or_default();
        if let Some(exception) = kind.strip_prefix("exception.") {
            return Err(ProviderError::new(
                exception_class(exception),
                format!("{exception}: {}", body_message(&event.data)),
            ));
        }
        let data = json(event)?;
        let index = data["contentBlockIndex"]
            .as_u64()
            .and_then(|i| usize::try_from(i).ok())
            .unwrap_or(0);
        match kind {
            "contentBlockStart" => {
                if let Some(tool) = data.pointer("/start/toolUse") {
                    out.tool_start(
                        index,
                        tool["toolUseId"].as_str().unwrap_or_default(),
                        tool["name"].as_str().unwrap_or_default(),
                    );
                }
            }
            "contentBlockDelta" => delta(&data["delta"], index, out),
            "contentBlockStop" => out.tool_end(index),
            "messageStop" => {
                self.reason = Some(stop_reason(data["stopReason"].as_str().unwrap_or_default()));
            }
            // The usage frame follows `messageStop` and ends the response.
            "metadata" => {
                out.usage(usage_report(&data["usage"]));
                if self.reason.is_some() {
                    out.finish(self.reason);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn end(&mut self, out: &mut Normalizer) -> Result<(), ProviderError> {
        match self.reason {
            Some(reason) => {
                out.finish(Some(reason));
                Ok(())
            }
            None => Err(truncated()),
        }
    }
}

fn delta(delta: &Value, index: usize, out: &mut Normalizer) {
    if let Some(text) = delta["text"].as_str() {
        out.text(text);
    } else if let Some(input) = delta.pointer("/toolUse/input").and_then(Value::as_str) {
        out.tool_arguments(index, input);
    } else if let Some(text) = delta
        .pointer("/reasoningContent/text")
        .and_then(Value::as_str)
    {
        out.reasoning(text);
    }
}

fn stop_reason(reason: &str) -> StopReason {
    match reason {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" | "model_context_window_exceeded" => StopReason::MaxTokens,
        "guardrail_intervened" | "content_filtered" => StopReason::ContentFilter,
        _ => StopReason::EndTurn,
    }
}

/// The class of a stream exception (Bedrock's error shapes).
fn exception_class(exception: &str) -> ErrorClass {
    match exception {
        "throttlingException" => ErrorClass::RateLimit,
        "serviceUnavailableException" | "modelNotReadyException" => ErrorClass::Overloaded,
        "validationException" => ErrorClass::InvalidRequest,
        "accessDeniedException" => ErrorClass::Auth,
        _ => ErrorClass::ServerError,
    }
}

/// Converse's `inputTokens` excludes cache reads and writes.
fn usage_report(usage: &Value) -> UsageReport {
    UsageReport {
        input: count(usage, "/inputTokens"),
        output: count(usage, "/outputTokens"),
        cache_read: count(usage, "/cacheReadInputTokens"),
        cache_write: count(usage, "/cacheWriteInputTokens"),
        reasoning: None,
        cost_usd: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_thinking_is_a_model_field_and_raises_the_cap() {
        let request = Request {
            model: "eu.anthropic.claude-sonnet-4-5-20250929-v1:0".into(),
            reasoning: Some(ReasoningLevel::Low),
            max_output_tokens: Some(1_000),
            messages: vec![
                Message::text(Role::User, "hi"),
                Message::text(Role::User, ""),
            ],
            ..Request::default()
        };
        let body = encode(&request);
        assert_eq!(body["inferenceConfig"]["maxTokens"], 5_096);
        assert_eq!(
            body["additionalModelRequestFields"]["thinking"]["budget_tokens"],
            4_096
        );
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        let other = Request {
            model: "amazon.nova-pro-v1:0".into(),
            reasoning: Some(ReasoningLevel::Low),
            ..Request::default()
        };
        let body = encode(&other);
        assert!(body.get("additionalModelRequestFields").is_none());
        assert!(body.get("inferenceConfig").is_none());
    }
}
