//! `pre_tool_call` directives (spec: plugin-system, `pre_tool_call`
//! directives) and the tool dispatcher's hook seam.
//!
//! Callbacks answer `modify` (fields shallow-merged into the arguments,
//! which the dispatcher re-validates against the tool schema), `block`
//! (the tool does not run; the message is its result) or `approve`
//! (escalate to the human approval gate, which blocks when it errors, is
//! denied or times out). Whatever order they answer in, block beats
//! approve beats modify; several modifies merge in callback order.

use std::sync::Arc;

use pmt_tools::dispatch::{Escalation, HookCall, PreDecision, SeamFuture, ToolHooks};
use pmt_tools::result::Status;
use serde_json::{Map, Value, json};

use super::{Answer, Hooks, Payload};
use crate::host::CallInfo;
use crate::host::hook_world::Reply;

/// The combined `pre_tool_call` outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreTool {
    /// Run the call as is.
    Continue,
    /// Run it with these fields merged into the arguments.
    Modify(Map<String, Value>),
    /// Do not run it.
    Block(String),
    /// Ask a human first.
    Approve {
        /// Shown in the approval prompt.
        message: String,
        /// The approval rule (`plugin_rule:<key>`).
        rule_key: String,
    },
}

/// Combines the answers by precedence: block, then approve, then modify.
#[must_use]
pub fn combine(answers: &[Answer]) -> PreTool {
    let mut approve = None;
    let mut fields = Map::new();
    for answer in answers {
        match &answer.reply {
            Reply::Block(message) => return PreTool::Block(message.clone()),
            Reply::Approve(approval) if approve.is_none() => {
                approve = Some(PreTool::Approve {
                    message: approval.message.clone(),
                    rule_key: approval
                        .rule_key
                        .clone()
                        .unwrap_or_else(|| answer.plugin.clone()),
                });
            }
            Reply::Modify(text) => match serde_json::from_str::<Value>(text) {
                Ok(Value::Object(map)) => fields.extend(map),
                _ => {
                    return PreTool::Block(format!(
                        "pre_tool_call of plugin '{}' answered modify with something other than a JSON object",
                        answer.plugin
                    ));
                }
            },
            _ => {}
        }
    }
    match approve {
        Some(approve) => approve,
        None if !fields.is_empty() => PreTool::Modify(fields),
        None => PreTool::Continue,
    }
}

/// Where a session's hook seam finds the profile's current hooks: a fixed
/// registry, or the live one of an embedder that hot-reloads plugins
/// (spec: plugin-system, Hot reload respects the prompt-cache invariant:
/// hooks activate at once for running sessions).
pub trait HookSource: Send + Sync {
    /// The hooks to fire now.
    fn hooks(&self) -> Arc<Hooks>;
}

impl HookSource for Arc<Hooks> {
    fn hooks(&self) -> Arc<Hooks> {
        Arc::clone(self)
    }
}

/// The dispatcher's hook seam over a profile's plugin hooks.
#[derive(Clone)]
pub struct PluginToolHooks {
    source: Arc<dyn HookSource>,
    call: CallInfo,
}

impl std::fmt::Debug for PluginToolHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginToolHooks")
            .field("call", &self.call)
            .finish_non_exhaustive()
    }
}

impl PluginToolHooks {
    /// Hooks of one session (`call` carries its id, surface and cwd).
    #[must_use]
    pub fn new(hooks: Arc<Hooks>, call: CallInfo) -> Self {
        Self::live(Arc::new(hooks), call)
    }

    /// Hooks of one session read from `source` at every call.
    #[must_use]
    pub fn live(source: Arc<dyn HookSource>, call: CallInfo) -> Self {
        Self { source, call }
    }

    fn payload(call: &HookCall<'_>, extra: &[(&str, Value)]) -> Payload {
        let mut fields = Map::new();
        fields.insert("session_id".to_owned(), call.session_id.into());
        fields.insert("call_id".to_owned(), call.call_id.into());
        fields.insert("tool".to_owned(), call.tool.into());
        fields.insert("arguments".to_owned(), call.arguments.clone());
        for (key, value) in extra {
            fields.insert((*key).to_owned(), value.clone());
        }
        Payload::new(fields)
    }
}

impl ToolHooks for PluginToolHooks {
    fn pre_tool_call<'a>(&'a self, call: HookCall<'a>) -> SeamFuture<'a, PreDecision> {
        Box::pin(async move {
            let hooks = self.source.hooks();
            if !hooks.handles("pre_tool_call") {
                return PreDecision::Continue;
            }
            let payload = Self::payload(&call, &[]);
            let answers = hooks.fire("pre_tool_call", &payload, &self.call).await;
            match combine(&answers) {
                PreTool::Continue => PreDecision::Continue,
                PreTool::Modify(fields) => PreDecision::Modify(fields),
                PreTool::Block(message) => PreDecision::Block(message),
                PreTool::Approve { message, rule_key } => PreDecision::Escalate(Escalation {
                    rule_key,
                    reason: message,
                }),
            }
        })
    }

    fn post_tool_call<'a>(
        &'a self,
        call: HookCall<'a>,
        status: Status,
        duration_ms: u64,
    ) -> SeamFuture<'a, ()> {
        Box::pin(async move {
            let hooks = self.source.hooks();
            if !hooks.handles("post_tool_call") {
                return;
            }
            let extra = [
                ("status", json!(status.to_string())),
                ("duration_ms", json!(duration_ms)),
            ];
            let payload = Self::payload(&call, &extra);
            // Observers: answers are ignored and cannot change the result.
            hooks.fire("post_tool_call", &payload, &self.call).await;
        })
    }
}
