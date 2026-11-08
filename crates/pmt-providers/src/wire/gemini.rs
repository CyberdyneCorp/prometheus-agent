//! Google AI Studio's native format (`gemini_native`, spec:
//! model-providers, Gemini native and AWS Bedrock providers):
//! `streamGenerateContent?alt=sse`, whose events each carry a whole
//! `GenerateContentResponse` chunk.

use pmt_config::schema::ReasoningLevel;
use serde_json::{Map, Value, json};

use super::{count, json, system_text, truncated};
use crate::classify::{ProviderError, body_message, classify_stream_error};
use crate::event::{Normalizer, StopReason, UsageReport};
use crate::request::{Message, Part, Request, Role};
use crate::sse::SseEvent;

/// The signature Google documents for function calls whose real thought
/// signature is not kept: Gemini 3 refuses history function calls without
/// one. The history keeps no signatures, so every replayed call has this.
const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

pub(super) fn encode(request: &Request) -> Value {
    let mut body = Map::new();
    if let Some(system) = system_text(request) {
        body.insert(
            "systemInstruction".into(),
            json!({"parts": [{"text": system}]}),
        );
    }
    body.insert("contents".into(), Value::Array(contents(request)));
    if !request.tools.is_empty() {
        let declarations: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                // `parametersJsonSchema` takes full JSON Schema (the older
                // `parameters` field takes only an OpenAPI subset).
                json!({"name": t.name, "description": t.description, "parametersJsonSchema": t.parameters})
            })
            .collect();
        body.insert(
            "tools".into(),
            json!([{ "functionDeclarations": declarations }]),
        );
    }
    let mut config = Map::new();
    if let Some(max) = request.max_output_tokens {
        config.insert("maxOutputTokens".into(), json!(max));
    }
    if let Some(thinking) = request
        .reasoning
        .and_then(|level| thinking(&request.model, level))
    {
        config.insert("thinkingConfig".into(), thinking);
    }
    if !config.is_empty() {
        body.insert("generationConfig".into(), Value::Object(config));
    }
    Value::Object(body)
}

/// The transcript as `contents`: tool results are user turns, and
/// adjacent turns of one role are merged (Gemini alternates roles).
fn contents(request: &Request) -> Vec<Value> {
    let gemini3 = model_name(&request.model).starts_with("gemini-3");
    let mut out: Vec<(&'static str, Vec<Value>)> = Vec::new();
    for message in &request.messages {
        let role = if message.role == Role::Assistant {
            "model"
        } else {
            "user"
        };
        let parts = parts(message, request, gemini3);
        if parts.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some((last, existing)) if *last == role => existing.extend(parts),
            _ => out.push((role, parts)),
        }
    }
    out.into_iter()
        .map(|(role, parts)| json!({"role": role, "parts": parts}))
        .collect()
}

fn parts(message: &Message, request: &Request, gemini3: bool) -> Vec<Value> {
    let mut signed = false;
    message
        .parts
        .iter()
        .map(|part| match part {
            Part::Text { text } => json!({"text": text}),
            Part::Image {
                media_type,
                data,
                is_url: true,
            } => json!({"fileData": {"mimeType": media_type, "fileUri": data}}),
            Part::Image {
                media_type, data, ..
            } => json!({"inlineData": {"mimeType": media_type, "data": data}}),
            Part::ToolCall {
                id,
                name,
                arguments,
            } => {
                let args: Value = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
                let mut call = json!({"functionCall": {"id": id, "name": name, "args": args}});
                // Only the first call of a turn carries the signature.
                if gemini3 && !std::mem::replace(&mut signed, true) {
                    call["thoughtSignature"] = json!(SKIP_SIGNATURE);
                }
                call
            }
            Part::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                let key = if *is_error { "error" } else { "content" };
                json!({"functionResponse": {
                    "id": call_id,
                    "name": tool_name(request, call_id),
                    "response": { key: content },
                }})
            }
        })
        .collect()
}

/// The name of the tool `call_id` called (a function response names it).
fn tool_name<'a>(request: &'a Request, call_id: &str) -> &'a str {
    request
        .messages
        .iter()
        .flat_map(|m| &m.parts)
        .find_map(|part| match part {
            Part::ToolCall { id, name, .. } if id == call_id => Some(name.as_str()),
            _ => None,
        })
        .unwrap_or_default()
}

fn model_name(model: &str) -> String {
    model
        .strip_prefix("models/")
        .unwrap_or(model)
        .to_ascii_lowercase()
}

/// The `thinkingConfig` of `level` on `model`: a thinking level on Gemini
/// 3, a token budget within the model's range on Gemini 2.5, and nothing
/// on models without thinking.
fn thinking(model: &str, level: ReasoningLevel) -> Option<Value> {
    let name = model_name(model);
    if name.starts_with("gemini-3") {
        let level = thinking_level(&name, level);
        return Some(json!({"thinkingLevel": level, "includeThoughts": true}));
    }
    if !name.starts_with("gemini-2.5") {
        return None;
    }
    let pro = name.starts_with("gemini-2.5-pro");
    let (floor, ceiling) = if pro { (128, 32_768) } else { (0, 24_576) };
    let wanted: u64 = match level {
        ReasoningLevel::None => 0,
        ReasoningLevel::Minimal => 1_024,
        ReasoningLevel::Low => 4_096,
        ReasoningLevel::Medium => 8_192,
        ReasoningLevel::High => 16_384,
        ReasoningLevel::Xhigh | ReasoningLevel::Max | ReasoningLevel::Ultra => 32_768,
    };
    let budget = wanted.clamp(floor, ceiling);
    Some(json!({"thinkingBudget": budget, "includeThoughts": budget > 0}))
}

/// Gemini 3 Pro knows `low` and `high`; Flash also `minimal` and `medium`.
fn thinking_level(name: &str, level: ReasoningLevel) -> &'static str {
    let flash = name.contains("flash");
    match level {
        ReasoningLevel::None | ReasoningLevel::Minimal if flash => "minimal",
        ReasoningLevel::Medium if flash => "medium",
        ReasoningLevel::None
        | ReasoningLevel::Minimal
        | ReasoningLevel::Low
        | ReasoningLevel::Medium => "low",
        ReasoningLevel::High
        | ReasoningLevel::Xhigh
        | ReasoningLevel::Max
        | ReasoningLevel::Ultra => "high",
    }
}

#[derive(Debug, Default)]
pub(super) struct Decoder {
    reason: Option<StopReason>,
    calls: usize,
}

impl super::Decode for Decoder {
    fn event(&mut self, event: &SseEvent, out: &mut Normalizer) -> Result<(), ProviderError> {
        let data = json(event)?;
        if let Some(error) = data.get("error") {
            let class = classify_stream_error(&error.to_string());
            return Err(ProviderError::new(class, body_message(&event.data)));
        }
        if data.pointer("/promptFeedback/blockReason").is_some() {
            self.reason = Some(StopReason::ContentFilter);
        }
        let candidate = &data["candidates"][0];
        for part in candidate["content"]["parts"]
            .as_array()
            .into_iter()
            .flatten()
        {
            self.part(part, out);
        }
        if let Some(usage) = data.get("usageMetadata") {
            out.usage(usage_report(usage));
        }
        if let Some(reason) = candidate["finishReason"].as_str() {
            self.reason = Some(stop_reason(reason));
        }
        Ok(())
    }

    fn end(&mut self, out: &mut Normalizer) -> Result<(), ProviderError> {
        // The last chunk carries the finish reason; the stream then ends.
        match self.reason {
            Some(reason) => {
                out.finish(Some(reason));
                Ok(())
            }
            None => Err(truncated()),
        }
    }
}

impl Decoder {
    fn part(&mut self, part: &Value, out: &mut Normalizer) {
        if let Some(call) = part.get("functionCall") {
            // Gemini sends each call whole; ids are optional.
            let index = self.calls;
            self.calls += 1;
            let id = call["id"]
                .as_str()
                .map_or_else(|| format!("call_{}", ulid::Ulid::generate()), str::to_owned);
            out.tool_start(index, &id, call["name"].as_str().unwrap_or_default());
            let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
            out.tool_arguments(index, &args.to_string());
            out.tool_end(index);
            return;
        }
        let Some(text) = part["text"].as_str() else {
            return;
        };
        if part["thought"].as_bool() == Some(true) {
            out.reasoning(text);
        } else {
            out.text(text);
        }
    }
}

fn stop_reason(reason: &str) -> StopReason {
    match reason {
        "MAX_TOKENS" => StopReason::MaxTokens,
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY" => {
            StopReason::ContentFilter
        }
        "MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL" => StopReason::Error,
        _ => StopReason::EndTurn,
    }
}

/// `promptTokenCount` includes cached input; thinking tokens are billed
/// as output.
fn usage_report(usage: &Value) -> UsageReport {
    let prompt = count(usage, "/promptTokenCount").unwrap_or(0);
    let cached = count(usage, "/cachedContentTokenCount").unwrap_or(0);
    let thoughts = count(usage, "/thoughtsTokenCount").unwrap_or(0);
    UsageReport {
        input: Some(prompt.saturating_sub(cached)),
        output: Some(count(usage, "/candidatesTokenCount").unwrap_or(0) + thoughts),
        cache_read: Some(cached),
        cache_write: None,
        reasoning: (thoughts > 0).then_some(thoughts),
        cost_usd: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{Message, Tool};

    fn request(model: &str, reasoning: Option<ReasoningLevel>) -> Request {
        Request {
            model: model.to_owned(),
            reasoning,
            ..Request::default()
        }
    }

    #[test]
    fn reasoning_effort_maps_to_the_models_thinking_control() {
        let body = encode(&request("gemini-2.5-flash", Some(ReasoningLevel::High)));
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            json!({"thinkingBudget": 16_384, "includeThoughts": true})
        );
        // 2.5 Pro cannot turn thinking off: its floor is 128.
        let body = encode(&request("gemini-2.5-pro", Some(ReasoningLevel::None)));
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            128
        );
        let body = encode(&request(
            "gemini-3-pro-preview",
            Some(ReasoningLevel::Medium),
        ));
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "low"
        );
        let body = encode(&request("gemini-3-flash", Some(ReasoningLevel::Medium)));
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "medium"
        );
        // Models without thinking get no thinking control at all.
        let body = encode(&request("gemini-2.0-flash", Some(ReasoningLevel::High)));
        assert!(body.get("generationConfig").is_none(), "{body}");
        // No requested effort sends no control either.
        let body = encode(&request("gemini-2.5-flash", None));
        assert!(body.get("generationConfig").is_none(), "{body}");
    }

    #[test]
    fn tool_turns_are_function_calls_and_named_responses() {
        let mut req = request("gemini-3-pro-preview", None);
        req.system = vec![crate::request::SystemBlock {
            text: "be brief".into(),
            cache_breakpoint: false,
        }];
        req.tools = vec![Tool {
            name: "read_file".into(),
            description: "read".into(),
            parameters: json!({"type": "object"}),
        }];
        req.messages = vec![
            Message::text(Role::User, "hi"),
            Message {
                role: Role::Assistant,
                parts: vec![Part::ToolCall {
                    id: "c1".into(),
                    name: "read_file".into(),
                    arguments: "{\"path\":\"a\"}".into(),
                }],
                cache_breakpoint: false,
            },
            Message {
                role: Role::Tool,
                parts: vec![Part::ToolResult {
                    call_id: "c1".into(),
                    content: "body".into(),
                    is_error: false,
                }],
                cache_breakpoint: false,
            },
            Message::text(Role::User, "and?"),
        ];
        let body = encode(&req);
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be brief");
        let contents = body["contents"].as_array().unwrap();
        // The tool result and the next user message merge into one turn.
        assert_eq!(contents.len(), 3, "{body}");
        let call = &contents[1]["parts"][0];
        assert_eq!(call["functionCall"]["args"], json!({"path": "a"}));
        assert_eq!(call["thoughtSignature"], SKIP_SIGNATURE);
        let response = &contents[2]["parts"][0]["functionResponse"];
        assert_eq!(response["name"], "read_file");
        assert_eq!(response["response"]["content"], "body");
        assert_eq!(contents[2]["parts"][1]["text"], "and?");
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"],
            json!({"type": "object"})
        );
    }
}
