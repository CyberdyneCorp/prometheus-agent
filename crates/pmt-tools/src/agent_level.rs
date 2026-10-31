//! The registry entries of the agent-level tools `todo`, `clarify` and
//! `session_search` (spec: tool-system, Agent-level tools are owned by the
//! turn loop).
//!
//! They need session state only the turn loop holds (the todo list, the
//! surface's clarify channel, the session store), so their entries carry
//! [`crate::Executor::AgentLoop`]: the turn loop executes them
//! (`pmt_agent::tools`) and a dispatch through the generic path answers
//! `<name> must be handled by the agent loop`. Their argument contracts
//! live here so the model sees them whichever surface runs the loop.

use serde_json::json;

use crate::descriptor::{Effect, RegisteredTool, ResourceClaims, ToolDescriptor};

/// Most items of a todo list.
pub const TODO_MAX_ITEMS: usize = 256;
/// Most characters of one todo item.
pub const TODO_MAX_CONTENT: usize = 4_000;
/// Most questions of one `clarify` call.
pub const CLARIFY_MAX_QUESTIONS: usize = 5;
/// Most choices of one question.
pub const CLARIFY_MAX_CHOICES: usize = 4;
/// Most characters of one choice.
pub const CLARIFY_MAX_CHOICE_CHARS: usize = 8_000;
/// `session_search` results by default.
pub const SEARCH_DEFAULT_LIMIT: u64 = 10;
/// Most `session_search` results.
pub const SEARCH_MAX_LIMIT: u64 = 50;
/// Messages on each side of a hit by default (window mode).
pub const SEARCH_DEFAULT_WINDOW: u64 = 5;
/// Most messages on each side of a hit.
pub const SEARCH_MAX_WINDOW: u64 = 25;

/// The agent-level tools this build registers.
#[must_use]
pub fn tools() -> Vec<RegisteredTool> {
    vec![
        RegisteredTool::agent_loop(todo()),
        RegisteredTool::agent_loop(clarify()),
        RegisteredTool::agent_loop(session_search()),
    ]
}

fn todo() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "todo",
        "todo",
        "Keep a prioritized task list for this session. Call with no todos to read it. \
         With merge=false (default) the given todos replace the list; with merge=true items \
         are updated by id in the fields given and new ids are appended. List position is \
         priority. Every call returns the full list and its revision.",
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "maxItems": TODO_MAX_ITEMS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "content": {"type": "string", "maxLength": TODO_MAX_CONTENT},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"]},
                            "parent": {"type": "string", "description": "Id of the parent item."}
                        },
                        "required": ["id"],
                        "additionalProperties": false
                    }
                },
                "merge": {"type": "boolean", "description": "Update by id instead of replacing (default false)."}
            },
            "additionalProperties": false
        }),
    );
    d.effect = Effect::None;
    // Writes the session's list: two calls of one batch apply in order.
    d.claims = Some(ResourceClaims::exclusive("todo"));
    d
}

fn clarify() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "clarify",
        "clarify",
        "Ask the user up to 5 questions at once when you cannot proceed without their \
         answer. Each question may offer up to 4 choices (the user can always answer in \
         their own words) and may allow several choices. Waits for the answer.",
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": CLARIFY_MAX_QUESTIONS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {"type": "string"},
                            "choices": {"type": "array", "maxItems": CLARIFY_MAX_CHOICES, "items": {"type": "string", "maxLength": CLARIFY_MAX_CHOICE_CHARS}},
                            "multi": {"type": "boolean", "description": "Allow several choices."}
                        },
                        "required": ["question"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["questions"],
            "additionalProperties": false
        }),
    );
    d.effect = Effect::None;
    d.claims = Some(ResourceClaims {
        interactive: true,
        ..ResourceClaims::none()
    });
    d
}

fn session_search() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "session_search",
        "session_search",
        "Search this profile's past conversations. Returns matching messages with \
         snippets grouped by session. To read around a hit, pass session_id and \
         around_message_id (window messages on each side).",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer", "minimum": 1, "maximum": SEARCH_MAX_LIMIT},
                "sort": {"type": "string", "enum": ["relevance", "newest", "oldest"]},
                "after": {"type": "string", "description": "RFC 3339 time or YYYY-MM-DD."},
                "before": {"type": "string", "description": "RFC 3339 time or YYYY-MM-DD."},
                "role_filter": {"type": "array", "items": {"type": "string", "enum": ["user", "assistant", "tool"]}},
                "exclude_session_ids": {"type": "array", "items": {"type": "string"}},
                "detail": {"type": "string", "enum": ["adaptive", "full"]},
                "session_id": {"type": "string"},
                "around_message_id": {"type": "string"},
                "window": {"type": "integer", "minimum": 0, "maximum": SEARCH_MAX_WINDOW}
            },
            "additionalProperties": false
        }),
    );
    d.effect = Effect::None;
    d.idempotent = true;
    d.claims = Some(ResourceClaims::none());
    d
}
