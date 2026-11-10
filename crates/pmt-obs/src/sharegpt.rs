//! `ShareGPT` trajectory export (spec: observability, `ShareGPT`
//! trajectory export): one session as a `conversations` record (`from` is `system`,
//! `human`, `gpt` or `tool`) a JSON-lines dataset loader reads. Assistant
//! tool calls become `<tool_call>{"name", "arguments"}</tool_call>`
//! blocks in the `gpt` value, reasoning a leading `<think>…</think>`
//! block, and tool results `<tool_response>{"tool_call_id", "name",
//! "content"}</tool_response>` in a `tool` turn. Only the session's
//! frozen system prompt is kept: per-request ephemeral overlays and
//! grader material never reach the trajectory or the prompt sections.

use serde::Serialize;
use serde_json::Value;

use crate::trajectory::events::Event;
use crate::trajectory::{Decoded, Line};

/// One turn of a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Turn {
    /// `system`, `human`, `gpt` or `tool`.
    pub from: &'static str,
    /// Its text.
    pub value: String,
}

/// What the record says about its session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Meta {
    /// The session.
    pub session_id: String,
    /// The model it started with.
    pub model: Option<String>,
    /// It ended without an error.
    pub completed: bool,
    /// The toolsets it ran with.
    pub toolsets: Vec<String>,
}

/// One session's record.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Record {
    /// The conversation.
    pub conversations: Vec<Turn>,
    /// The tool definitions offered.
    pub tools: Vec<Value>,
    /// About the session.
    pub meta: Meta,
}

/// Whether the session ended without an error: its last turn completed
/// and it did not end with an error reason.
#[must_use]
pub fn completed(lines: &[Line]) -> bool {
    let mut last_turn: Option<String> = None;
    let mut ended_badly = false;
    for line in lines {
        if let Ok(Decoded::Known(event)) = line.decode() {
            match *event {
                Event::TurnEnd(end) => last_turn = Some(end.reason),
                Event::SessionEnd(end) => ended_badly = end.reason == "error",
                _ => {}
            }
        }
    }
    !ended_badly && last_turn.as_deref() == Some("completed")
}

/// A `<tool_call>` body, fields in the documented order.
#[derive(Serialize)]
struct CallBlock<'a> {
    name: &'a str,
    arguments: &'a Value,
}

/// A `<tool_response>` body, fields in the documented order.
#[derive(Serialize)]
struct ResponseBlock<'a> {
    tool_call_id: &'a str,
    name: &'a str,
    content: &'a str,
}

fn ordered(block: &impl Serialize) -> String {
    serde_json::to_string(block).unwrap_or_default()
}

#[derive(Default)]
struct Builder {
    turns: Vec<Turn>,
    think: Option<String>,
}

impl Builder {
    fn push(&mut self, from: &'static str, value: String) {
        self.turns.push(Turn { from, value });
    }

    /// Text of the next `gpt` turn, after any pending reasoning.
    fn gpt(&mut self, text: &str) {
        let value = match self.think.take() {
            Some(think) => format!("<think>{think}</think>\n{text}"),
            None => text.to_owned(),
        };
        self.push("gpt", value);
    }

    fn tool_call(&mut self, name: &str, arguments: &Value) {
        let block = format!(
            "<tool_call>{}</tool_call>",
            ordered(&CallBlock { name, arguments })
        );
        match self.turns.last_mut() {
            Some(last) if last.from == "gpt" && self.think.is_none() => {
                if !last.value.is_empty() {
                    last.value.push('\n');
                }
                last.value.push_str(&block);
            }
            _ => self.gpt(&block),
        }
    }

    fn tool_result(&mut self, call_id: &str, name: &str, content: &str) {
        let block = format!(
            "<tool_response>{}</tool_response>",
            ordered(&ResponseBlock {
                tool_call_id: call_id,
                name,
                content,
            })
        );
        match self.turns.last_mut() {
            Some(last) if last.from == "tool" => {
                last.value.push('\n');
                last.value.push_str(&block);
            }
            _ => self.push("tool", block),
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::UserMessage(m) => self.push("human", m.text),
            Event::AssistantReasoning(r) => {
                if let Some(text) = r.text.filter(|t| !t.trim().is_empty()) {
                    self.think = Some(text);
                }
            }
            Event::AssistantMessage(m) => self.gpt(&m.text),
            Event::ToolCall(call) => self.tool_call(&call.tool, &call.arguments),
            Event::ToolResult(result) => {
                let content = match (&result.output, &result.error) {
                    (Some(output), _) => output.clone(),
                    (None, Some(error)) => format!("{}: {}", error.kind, error.message),
                    (None, None) => String::new(),
                };
                self.tool_result(&result.call_id, &result.tool, &content);
            }
            _ => {}
        }
    }
}

/// The record of the session `lines` describe, with its frozen `system`
/// prompt and the `tools` it was offered.
#[must_use]
pub fn record(
    lines: &[Line],
    system: Option<&str>,
    tools: Vec<Value>,
    toolsets: Vec<String>,
) -> Record {
    let mut builder = Builder::default();
    if let Some(system) = system.filter(|s| !s.trim().is_empty()) {
        builder.push("system", system.to_owned());
    }
    let mut model = None;
    for line in lines {
        let Ok(Decoded::Known(event)) = line.decode() else {
            continue;
        };
        if let Event::SessionStart(start) = &*event {
            model = model.or_else(|| start.model.clone());
        }
        builder.event(*event);
    }
    Record {
        conversations: builder.turns,
        tools,
        meta: Meta {
            session_id: lines
                .first()
                .map(|l| l.session_id.clone())
                .unwrap_or_default(),
            model,
            completed: completed(lines),
            toolsets,
        },
    }
}
