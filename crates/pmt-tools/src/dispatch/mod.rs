//! The dispatch pipeline (spec: tool-system, Dispatch pipeline; Typed tool
//! result contract; Error bodies are bounded and sanitized; Agent-level
//! tools are owned by the turn loop; Parallel and timed execution).
//!
//! Every model tool call goes through the same steps, in order: resolve
//! the name against the session's frozen surface, parse the arguments,
//! coerce string scalars, validate against the schema, `pre_tool_call`
//! hooks, capability re-check, approval gate, execute (bounded by the
//! tool's timeout, cancellable, panics isolated), `post_tool_call` hooks,
//! `transform_tool_result` hooks, then the result budget. Each step that
//! stops the call produces a typed result instead; nothing a handler does
//! escapes the dispatcher. Error text is sanitized and bounded before the
//! model sees it. Provenance tagging (M2) attaches after the budget.

mod batch;
pub mod seams;

use std::sync::Arc;
use std::time::Duration;

use pmt_core::cancel::CancelToken;
use pmt_core::panic::{CrashReporter, isolate};
use serde_json::Value;
use tokio::time::Instant;

pub use self::seams::{
    AllowAll, ApprovalGate, Capabilities, DispatchObserver, GateRequest, GateVerdict, HookCall,
    NoHooks, NoObserver, PreDecision, SeamFuture, ToolHooks,
};
use crate::args;
use crate::budget::ResultBudget;
use crate::descriptor::{CallContext, Executor, RegisteredTool, ToolFailure};
use crate::result::{Content, Status, ToolResult};
use crate::sanitize;
use crate::surface::ToolSurface;

/// One tool call as the model made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// The model's call id.
    pub call_id: String,
    /// Tool name.
    pub name: String,
    /// Argument JSON text.
    pub arguments: String,
}

impl ToolCall {
    /// A call of `name` with `arguments` JSON text.
    #[must_use]
    pub fn new(call_id: &str, name: &str, arguments: &str) -> Self {
        Self {
            call_id: call_id.to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        }
    }
}

/// Everything a session's dispatcher works with.
pub struct DispatchSetup {
    /// Session id.
    pub session_id: String,
    /// The session's frozen surface.
    pub surface: Arc<ToolSurface>,
    /// The session's effective capability set.
    pub capabilities: Arc<dyn Capabilities>,
    /// Tool-call hooks.
    pub hooks: Arc<dyn ToolHooks>,
    /// The approval gate.
    pub gate: Arc<dyn ApprovalGate>,
    /// Trajectory events.
    pub observer: Arc<dyn DispatchObserver>,
    /// The session's result budget.
    pub budget: ResultBudget,
    /// Where handler panics are reported.
    pub reporter: Arc<CrashReporter>,
    /// `tools.default_timeout_secs`.
    pub default_timeout: Duration,
    /// `agent.tool_concurrency`.
    pub concurrency: usize,
}

/// Runs a session's tool calls. Cheap to clone.
#[derive(Clone)]
pub struct Dispatcher {
    setup: Arc<DispatchSetup>,
}

impl std::fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatcher")
            .field("session_id", &self.setup.session_id)
            .finish_non_exhaustive()
    }
}

/// A call that ran through the pipeline, before the per-turn budget.
struct Executed {
    tool: Option<Arc<RegisteredTool>>,
    result: ToolResult,
    duration_ms: u64,
}

impl Dispatcher {
    /// A dispatcher for one session.
    #[must_use]
    pub fn new(setup: DispatchSetup) -> Self {
        Self {
            setup: Arc::new(setup),
        }
    }

    /// The session's surface.
    #[must_use]
    pub fn surface(&self) -> &ToolSurface {
        &self.setup.surface
    }

    /// Dispatches one call (a batch of one).
    pub async fn dispatch(&self, call: ToolCall, cancel: &CancelToken) -> ToolResult {
        self.run_batch(vec![call], cancel)
            .await
            .pop()
            .unwrap_or_else(|| ToolResult::error("no result"))
    }

    /// Runs one call through the pipeline up to the per-result budget.
    async fn execute(&self, call: &ToolCall, cancel: &CancelToken) -> Executed {
        let setup = &self.setup;
        setup
            .observer
            .tool_call(&call.call_id, &call.name, &call.arguments);
        let started = Instant::now();
        let tool = setup.surface.get(&call.name).cloned();
        let (arguments, result) = tokio::select! {
            biased;
            () = cancel.cancelled() => (raw(call), cancelled(&call.name)),
            outcome = self.pipeline(call, tool.as_deref(), cancel) => outcome,
        };
        let mut result = bounded(&call.name, result);
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let hook_call = HookCall {
            session_id: &setup.session_id,
            call_id: &call.call_id,
            tool: &call.name,
            arguments: &arguments,
        };
        setup
            .hooks
            .post_tool_call(hook_call, result.status, duration_ms)
            .await;
        result = setup.hooks.transform_tool_result(hook_call, result).await;
        setup
            .budget
            .apply(&call.call_id, tool.as_deref(), &mut result);
        Executed {
            tool,
            result,
            duration_ms,
        }
    }

    /// Resolve, parse, validate, hooks, capabilities, gate, execute.
    /// Returns the arguments hooks see and the result.
    async fn pipeline(
        &self,
        call: &ToolCall,
        tool: Option<&RegisteredTool>,
        cancel: &CancelToken,
    ) -> (Value, ToolResult) {
        let Some(tool) = tool else {
            return (
                raw(call),
                ToolResult::error(format!("unknown tool: {}", call.name)),
            );
        };
        let arguments = match checked_arguments(call, tool) {
            Ok(arguments) => arguments,
            Err(result) => return (raw(call), result),
        };
        let result = match self.admit(call, tool, &arguments).await {
            Ok(()) => self.run(call, tool, arguments.clone(), cancel).await,
            Err(result) => result,
        };
        (arguments, result)
    }

    /// `pre_tool_call` hooks, capability re-check and approval gate.
    async fn admit(
        &self,
        call: &ToolCall,
        tool: &RegisteredTool,
        arguments: &Value,
    ) -> Result<(), ToolResult> {
        let setup = &self.setup;
        let hook_call = HookCall {
            session_id: &setup.session_id,
            call_id: &call.call_id,
            tool: &call.name,
            arguments,
        };
        if let PreDecision::Block(message) = setup.hooks.pre_tool_call(hook_call).await {
            return Err(ToolResult::text(Status::Blocked, message));
        }
        let held = setup
            .capabilities
            .current()
            .map_err(|err| ToolResult::text(Status::Denied, err.message()))?;
        if let Some(missing) = tool
            .descriptor
            .capabilities
            .iter()
            .find(|c| !held.allows(c))
        {
            let err = pmt_core::capability::not_granted(missing);
            return Err(ToolResult::text(Status::Denied, err.message()));
        }
        let request = GateRequest {
            session_id: &setup.session_id,
            call_id: &call.call_id,
            tool,
            arguments,
        };
        match setup.gate.check(request).await {
            GateVerdict::Allow => Ok(()),
            GateVerdict::Blocked(message) => Err(ToolResult::text(Status::Blocked, message)),
            GateVerdict::Denied(message) => Err(ToolResult::text(Status::Denied, message)),
        }
    }

    /// Executes the handler under the timeout, panic isolation and the
    /// cancel token.
    async fn run(
        &self,
        call: &ToolCall,
        tool: &RegisteredTool,
        arguments: Value,
        cancel: &CancelToken,
    ) -> ToolResult {
        let name = tool.name();
        let handler = match &tool.executor {
            Executor::Handler(handler) => Arc::clone(handler),
            Executor::AgentLoop => {
                return ToolResult::error(format!("{name} must be handled by the agent loop"));
            }
        };
        let context = CallContext {
            call_id: call.call_id.clone(),
            session_id: self.setup.session_id.clone(),
            cancel: cancel.child(),
        };
        let limit = tool
            .descriptor
            .timeout
            .unwrap_or(self.setup.default_timeout);
        let task = format!("tool:{name}");
        let work = isolate(
            &task,
            &self.setup.reporter,
            handler.call(&context, arguments),
        );
        match tokio::time::timeout(limit, work).await {
            Err(_) => ToolResult::text(
                Status::Timeout,
                format!("{name} timed out after {}s", limit.as_secs()),
            ),
            Ok(Err(panic)) => {
                let detail = panic.hint().unwrap_or(panic.message()).to_owned();
                tool_error(name, &ToolFailure::new("panic", detail))
            }
            Ok(Ok(Err(failure))) => tool_error(name, &failure),
            Ok(Ok(Ok(result))) => result,
        }
    }
}

/// Parses, coerces and validates the call's arguments.
fn checked_arguments(call: &ToolCall, tool: &RegisteredTool) -> Result<Value, ToolResult> {
    let map = args::parse(&call.arguments).map_err(|message| {
        ToolResult::error(format!("invalid arguments for {}: {message}", call.name))
    })?;
    let mut arguments = Value::Object(map);
    let schema = &tool.descriptor.parameters;
    args::coerce(schema, &mut arguments);
    let violations = args::validate(schema, &arguments);
    if violations.is_empty() {
        return Ok(arguments);
    }
    let lines: Vec<String> = violations.iter().map(ToString::to_string).collect();
    Err(ToolResult::error(format!(
        "invalid arguments for {}:\n{}",
        call.name,
        lines.join("\n")
    )))
}

/// The raw argument text, for hooks of calls whose arguments did not
/// parse.
fn raw(call: &ToolCall) -> Value {
    Value::String(call.arguments.clone())
}

fn cancelled(name: &str) -> ToolResult {
    ToolResult::text(Status::Cancelled, format!("{name} was cancelled"))
}

/// `[TOOL_ERROR] <tool> failed: <kind>: <message>`.
fn tool_error(name: &str, failure: &ToolFailure) -> ToolResult {
    ToolResult::error(format!(
        "[TOOL_ERROR] {name} failed: {}: {}",
        failure.kind, failure.message
    ))
}

/// Sanitizes and bounds the text of every unsuccessful result, logging
/// the original at DEBUG.
fn bounded(name: &str, mut result: ToolResult) -> ToolResult {
    if result.status.succeeded() {
        return result;
    }
    for part in &mut result.content {
        if let Content::Text { text } = part {
            sanitize::log_error(name, text);
            *text = sanitize::error_text(text);
        }
    }
    result
}

#[cfg(test)]
mod tests;
