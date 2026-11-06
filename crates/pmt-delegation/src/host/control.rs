//! Control actions (spec: subagent-delegation, Control actions scoped to
//! the caller's spawn tree): `list`, `status`, `steer`, `stop` and `wait`
//! reach only the caller's own spawn tree and bypass the pause and depth
//! checks that guard spawns.

use std::sync::Arc;

use pmt_agent::turn::Issuer;
use pmt_core::error::codes;
use pmt_tools::ToolResult;
use serde_json::{Value, json};

use super::Delegator;
use crate::Refusal;
use crate::contract::{Action, Call};
use crate::tree::{Entry, Registry};

impl Delegator {
    pub(super) async fn control(&self, call: &Call) -> Result<ToolResult, Refusal> {
        let conversation = self.conversation();
        let registry = Registry::process();
        if call.action == Action::List {
            let children: Vec<Value> = registry
                .tree(&conversation)
                .iter()
                .map(|e| e.view())
                .collect();
            return Ok(ok(&json!({ "subagents": children })));
        }
        let id = call.subagent_id.as_deref().ok_or_else(|| {
            Refusal::new(
                &codes::DELEGATION_INVALID_TASK,
                format!("action={} needs a subagent_id.", call.action.as_str()),
            )
        })?;
        let entry = registry.in_tree(&conversation, id)?;
        match call.action {
            Action::Steer => steer_entry(&entry, call.message.as_deref()),
            Action::Stop => {
                entry.request_stop(Issuer::User);
                Ok(ok(
                    &json!({"subagent_id": id, "status": "interrupt_requested"}),
                ))
            }
            Action::Wait => {
                let result = entry.wait().await;
                Ok(ok(&json!({"subagent_id": id, "result": result})))
            }
            _ => Ok(ok(&entry.view())),
        }
    }
}

/// Queues `message` for `entry` (the `steer` action and the plugin
/// interface).
pub(super) fn steer_entry(
    entry: &Arc<Entry>,
    message: Option<&str>,
) -> Result<ToolResult, Refusal> {
    let Some(message) = message.filter(|m| !m.trim().is_empty()) else {
        return Err(Refusal::new(
            &codes::DELEGATION_INVALID_TASK,
            "action=steer needs a non-empty message.",
        ));
    };
    if matches!(entry.state(), crate::tree::State::Ended(_)) {
        return Err(crate::tree::not_in_tree(&entry.subagent_id));
    }
    entry.steers.push(message);
    Ok(ok(
        &json!({"subagent_id": entry.subagent_id, "status": "queued"}),
    ))
}

fn ok(value: &Value) -> ToolResult {
    ToolResult::ok(value.to_string())
}
