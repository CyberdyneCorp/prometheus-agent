//! The registry entries of the `kanban` toolset (spec: kanban, Kanban
//! toolset and grants): the turn loop runs them through the session's
//! kanban port. A worker session gets them automatically, scoped to its
//! own board and without `kanban_list` and `kanban_unblock`; any other
//! session only when `kanban` is listed in its toolsets.

use serde_json::{Value, json};

use crate::descriptor::{Effect, RegisteredTool, ResourceClaims, ToolDescriptor};

/// The toolset.
pub const TOOLSET: &str = "kanban";

/// Every kanban tool, in toolset order.
pub const TOOLS: &[&str] = &[
    "kanban_show",
    "kanban_list",
    "kanban_create",
    "kanban_comment",
    "kanban_attach",
    "kanban_link",
    "kanban_complete",
    "kanban_block",
    "kanban_schedule",
    "kanban_heartbeat",
    "kanban_request_review",
    "kanban_request_changes",
    "kanban_unblock",
    "kanban_attachments",
];

/// Tools a worker's grant leaves out.
pub const NOT_WORKER: &[&str] = &["kanban_list", "kanban_unblock"];

/// Tools that act on the caller's own run and refuse outside a worker
/// session.
pub const RUN_LIFECYCLE: &[&str] = &[
    "kanban_complete",
    "kanban_block",
    "kanban_schedule",
    "kanban_heartbeat",
    "kanban_request_review",
];

fn board() -> Value {
    json!({"type": "string", "description": "The board (default: the worker's own board, else the profile's current board)."})
}

fn task_id() -> Value {
    json!({"type": "string", "description": "The task id (t_xxxxxxxx; default: the worker's own task)."})
}

fn tool(
    name: &str,
    description: &str,
    properties: &Value,
    required: &[&str],
    effect: Effect,
) -> RegisteredTool {
    let mut d = ToolDescriptor::new(
        name,
        TOOLSET,
        description,
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        }),
    );
    d.effect = effect;
    d.claims = Some(ResourceClaims::none());
    RegisteredTool::agent_loop(d)
}

fn reads() -> Vec<RegisteredTool> {
    vec![
        tool(
            "kanban_show",
            "Show a kanban task: status, body, parents and their results, comments, attachments and runs.",
            &json!({"board": board(), "task_id": task_id()}),
            &[],
            Effect::None,
        ),
        tool(
            "kanban_list",
            "List a board's tasks by priority then age.",
            &json!({"board": board(), "status": {"type": "string", "description": "Only this status."}, "assignee": {"type": "string"}}),
            &[],
            Effect::None,
        ),
        tool(
            "kanban_attachments",
            "List a task's attachments: id, filename, content_type, size, uploader and a read-only local path your file tools may read.",
            &json!({"board": board(), "task_id": task_id()}),
            &[],
            Effect::None,
        ),
    ]
}

fn edits() -> Vec<RegisteredTool> {
    vec![
        tool(
            "kanban_create",
            "Create a follow-up task (it waits in todo while a parent is open). List the ids you create in kanban_complete's created_tasks.",
            &json!({
                "board": board(),
                "title": {"type": "string"},
                "body": {"type": "string", "description": "Self-contained instructions and acceptance criteria."},
                "assignee": {"type": "string", "description": "An existing profile."},
                "parents": {"type": "array", "items": {"type": "string"}},
                "priority": {"type": "integer"},
                "idempotency_key": {"type": "string"}
            }),
            &["title"],
            Effect::External,
        ),
        tool(
            "kanban_comment",
            "Add a comment to a task.",
            &json!({"board": board(), "task_id": task_id(), "body": {"type": "string"}}),
            &["body"],
            Effect::External,
        ),
        tool(
            "kanban_attach",
            "Attach a file you can read to a task (a worker may attach only to its own task).",
            &json!({"board": board(), "task_id": task_id(), "path": {"type": "string"}}),
            &["path"],
            Effect::External,
        ),
        tool(
            "kanban_link",
            "Make child depend on parent (same board; no cycles).",
            &json!({"board": board(), "parent": {"type": "string"}, "child": {"type": "string"}}),
            &["parent", "child"],
            Effect::External,
        ),
        tool(
            "kanban_unblock",
            "Lift a task's block or schedule.",
            &json!({"board": board(), "task_id": {"type": "string"}}),
            &["task_id"],
            Effect::External,
        ),
        tool(
            "kanban_request_changes",
            "As the reviewer, send a task in review back with a comment.",
            &json!({"board": board(), "task_id": task_id(), "comment": {"type": "string"}}),
            &["comment"],
            Effect::External,
        ),
    ]
}

fn lifecycle() -> Vec<RegisteredTool> {
    vec![
        tool(
            "kanban_complete",
            "Finish your task with evidence: a result or summary that shows what was done. created_tasks lists task ids you created.",
            &json!({
                "result": {"type": "string"},
                "summary": {"type": "string"},
                "created_tasks": {"type": "array", "items": {"type": "string"}}
            }),
            &[],
            Effect::External,
        ),
        tool(
            "kanban_block",
            "Stop working your task: kind dependency, needs_input, capability, budget or transient, with a reason a person can act on.",
            &json!({
                "kind": {"type": "string", "enum": ["dependency", "needs_input", "capability", "budget", "transient"]},
                "reason": {"type": "string"}
            }),
            &["kind", "reason"],
            Effect::External,
        ),
        tool(
            "kanban_schedule",
            "Park your task until a time (RFC 3339), ending this run.",
            &json!({"wake_at": {"type": "string", "description": "RFC 3339 instant; omit to park until unblocked."}}),
            &[],
            Effect::External,
        ),
        tool(
            "kanban_heartbeat",
            "Tell the board you are still working (renews your claim); an optional short note is logged.",
            &json!({"note": {"type": "string"}}),
            &[],
            Effect::External,
        ),
        tool(
            "kanban_request_review",
            "Hand your task to a reviewer with a summary.",
            &json!({"summary": {"type": "string"}}),
            &[],
            Effect::External,
        ),
    ]
}

/// The kanban tools this build registers.
#[must_use]
pub fn tools() -> Vec<RegisteredTool> {
    let mut all = reads();
    all.extend(edits());
    all.extend(lifecycle());
    all.sort_by_key(|t| {
        TOOLS
            .iter()
            .position(|n| *n == t.descriptor.name)
            .unwrap_or(usize::MAX)
    });
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_tool_is_registered_once() {
        let names: Vec<String> = tools()
            .into_iter()
            .map(|t| t.descriptor.name.clone())
            .collect();
        assert_eq!(
            names,
            TOOLS.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()
        );
    }
}
