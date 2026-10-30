//! What the dispatcher calls out to: hooks, the approval gate, the
//! session's capability set and the trajectory observer. Each has a no-op
//! (or pass-through) implementation until its owner wires the real one:
//! plugin hooks (plugin-system), the command approval gate ([`crate::approval`]), live
//! session grants (security-boundaries) and trajectory recording (the
//! turn loop).

use std::future::Future;
use std::pin::Pin;

use pmt_config::grants::{SessionGrants, Subject};
use pmt_core::capability::CapabilitySet;
use pmt_core::error::Error;
use serde_json::Value;

use crate::descriptor::RegisteredTool;
use crate::result::{Status, ToolResult};

/// A boxed future returned by the seams.
pub type SeamFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The call a hook sees.
#[derive(Debug, Clone, Copy)]
pub struct HookCall<'a> {
    /// Session id.
    pub session_id: &'a str,
    /// The model's call id.
    pub call_id: &'a str,
    /// Tool name as called.
    pub tool: &'a str,
    /// Validated arguments (or the raw text when they did not parse).
    pub arguments: &'a Value,
}

/// A `pre_tool_call` decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreDecision {
    /// Run the call.
    Continue,
    /// Do not run it; the message becomes the result.
    Block(String),
    /// Run it only after a human approval (the approval gate asks; with no
    /// channel the call is denied naming the rule).
    Escalate(Escalation),
}

/// A plugin rule asking for human approval of one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Escalation {
    /// The plugin's rule key; approvals record it as `plugin_rule:<key>`.
    pub rule_key: String,
    /// Why the rule wants a human to look.
    pub reason: String,
}

/// Tool-call hooks (`pre_tool_call`, `post_tool_call`,
/// `transform_tool_result`).
pub trait ToolHooks: Send + Sync {
    /// Before the capability re-check and approval gate.
    fn pre_tool_call<'a>(&'a self, call: HookCall<'a>) -> SeamFuture<'a, PreDecision> {
        let _ = call;
        Box::pin(async { PreDecision::Continue })
    }

    /// After every call, with its status and integer duration.
    fn post_tool_call<'a>(
        &'a self,
        call: HookCall<'a>,
        status: Status,
        duration_ms: u64,
    ) -> SeamFuture<'a, ()> {
        let _ = (call, status, duration_ms);
        Box::pin(async {})
    }

    /// Rewrites a result before the result budget.
    fn transform_tool_result<'a>(
        &'a self,
        call: HookCall<'a>,
        result: ToolResult,
    ) -> SeamFuture<'a, ToolResult> {
        let _ = call;
        Box::pin(async move { result })
    }
}

/// No hooks.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHooks;

impl ToolHooks for NoHooks {}

/// What the approval gate is asked.
#[derive(Debug, Clone, Copy)]
pub struct GateRequest<'a> {
    /// Session id.
    pub session_id: &'a str,
    /// The model's call id.
    pub call_id: &'a str,
    /// The tool.
    pub tool: &'a RegisteredTool,
    /// Validated arguments.
    pub arguments: &'a Value,
    /// A `pre_tool_call` hook's escalation, when one asked for approval.
    pub escalation: Option<&'a Escalation>,
}

/// The approval gate's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    /// Run the call.
    Allow,
    /// Never run (hardline, deny rule): `status = "blocked"`.
    Blocked(String),
    /// Refused (denied approval, unattended deny): `status = "denied"`.
    Denied(String),
}

/// The approval gate.
pub trait ApprovalGate: Send + Sync {
    /// Decides whether the call may run.
    fn check<'a>(&'a self, request: GateRequest<'a>) -> SeamFuture<'a, GateVerdict>;
}

/// Allows every call, escalations included (tests and tools-only setups;
/// sessions use [`crate::approval::CommandGate`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

impl ApprovalGate for AllowAll {
    fn check<'a>(&'a self, _: GateRequest<'a>) -> SeamFuture<'a, GateVerdict> {
        Box::pin(async { GateVerdict::Allow })
    }
}

/// The session's effective capability set, read at every call so a
/// revocation applies to the next call.
pub trait Capabilities: Send + Sync {
    /// The current set.
    ///
    /// # Errors
    ///
    /// When it cannot be read (the call is then denied).
    fn current(&self) -> Result<CapabilitySet, Error>;
}

impl Capabilities for CapabilitySet {
    fn current(&self) -> Result<CapabilitySet, Error> {
        Ok(self.clone())
    }
}

impl Capabilities for SessionGrants {
    fn current(&self) -> Result<CapabilitySet, Error> {
        self.capabilities(&Subject::Profile)
    }
}

/// Receives exactly one `tool.call` and one `tool.result` per call.
pub trait DispatchObserver: Send + Sync {
    /// The call arrived (`arguments` as the model sent them).
    fn tool_call(&self, call_id: &str, tool: &str, arguments: &str);
    /// The final result (after hooks and budgets).
    fn tool_result(&self, call_id: &str, tool: &str, result: &ToolResult, duration_ms: u64);
}

/// Records nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoObserver;

impl DispatchObserver for NoObserver {
    fn tool_call(&self, _: &str, _: &str, _: &str) {}
    fn tool_result(&self, _: &str, _: &str, _: &ToolResult, _: u64) {}
}
