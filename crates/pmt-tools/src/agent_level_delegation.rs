//! The registry entries of `delegate_task` and `execute_code` (spec:
//! subagent-delegation, `delegate_task` tool contract; Programmatic tool
//! calling under the caller's grant). Both run in the turn loop, which
//! holds the session's children and grant. Task items are not closed in
//! the schema: `delegate_task` itself refuses unknown task fields with a
//! message naming the task, and a delegation router may add typed task
//! parameters for the session.

use serde_json::json;

use crate::descriptor::{Effect, ResourceClaims, ToolDescriptor};

/// The `delegate_task` description before the session's agent listing.
pub const DELEGATE_DESCRIPTION: &str = "Delegate work to subagents: each task runs in an isolated child agent with its own \
     session and terminal, under a grant that can only be narrower than yours, and returns a \
     structured result (status, summary, artifacts, usage) instead of its transcript. Give each \
     task a concrete `goal` and the `context` it needs (children cannot ask questions). \
     Narrow a child with `toolsets`, pick a named `agent`, pin a `model`, or ask for a JSON \
     answer with `output_schema`. Other actions (`list`, `status`, `steer`, `stop`, `wait`) act \
     on children of this conversation by `subagent_id`.";

/// `delegate_task`.
#[must_use]
pub fn delegate_task() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "delegate_task",
        "delegation",
        DELEGATE_DESCRIPTION,
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["spawn", "list", "status", "steer", "stop", "wait"], "description": "Default spawn."},
                "tasks": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "goal": {"type": "string", "description": "What the child must achieve."},
                            "context": {"type": "string", "description": "Background the child needs."},
                            "agent": {"type": "string", "description": "A named agent definition."},
                            "toolsets": {"type": "array", "items": {"type": "string"}, "description": "Narrow the child's tools to these toolsets."},
                            "model": {"type": "string", "description": "provider/model for this child."},
                            "output_schema": {"type": "object", "description": "JSON Schema (2020-12) the child's final answer must match."},
                            "images": {"type": "array", "items": {"type": "string"}, "maxItems": 8},
                            "group": {"type": "string"},
                            "weight": {"type": "number", "description": "Share of the call budget under budget.split = weighted."}
                        },
                        "required": ["goal"]
                    }
                },
                "subagent_id": {"type": "string", "description": "The child a control action targets."},
                "message": {"type": "string", "description": "The steer message."},
                "join": {"type": "string", "enum": ["all"], "description": "Wait for every child (the only mode)."},
                "budget": {
                    "type": "object",
                    "properties": {
                        "max_cost_usd": {"type": "number"},
                        "max_tokens": {"type": "integer"},
                        "max_iterations": {"type": "integer"},
                        "split": {"type": "string", "enum": ["even", "weighted"]}
                    },
                    "additionalProperties": false
                }
            },
            "additionalProperties": false
        }),
    );
    d.effect = Effect::External;
    d.claims = Some(ResourceClaims::none());
    d
}

/// `execute_code`.
#[must_use]
pub fn execute_code() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "execute_code",
        "code_execution",
        "Run a Python script in a sandbox to process data or chain tool calls without round \
         trips. Inside it, `from prometheus_tools import <tool>` gives a function per tool you \
         may call from code (see `available` in an error); each returns the tool's text. \
         Print what you want back: stdout (capped) and stderr are returned. Imports and \
         variables persist between calls of this session; pass `reset: true` to start clean.",
        json!({
            "type": "object",
            "properties": {
                "code": {"type": "string", "description": "The Python source."},
                "reset": {"type": "boolean", "description": "Discard the imports and variables earlier calls left (they persist between calls by default)."}
            },
            "required": ["code"],
            "additionalProperties": false
        }),
    );
    d.effect = Effect::External;
    d.claims = Some(ResourceClaims::none());
    d
}
