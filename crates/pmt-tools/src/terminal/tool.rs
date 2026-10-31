//! The `terminal` and `process` tools over the session's
//! [`LocalTerminal`] (spec: execution-environments, Local backend runs
//! under the OS sandbox; Foreground result shaping; Background process
//! registry).
//!
//! The dispatcher has already run the approval gate on the command line
//! (the tools declare `process.spawn:*` and a `command` argument), so the
//! terminal's own guard chain keeps only the capability and workdir
//! checks.

use std::sync::Arc;

use pmt_core::capability::Capability;
use serde_json::{Value, json};

use super::process::{ACTIONS, ProcessArgs};
use super::result::{Status as TerminalStatus, TerminalResult};
use super::{LocalTerminal, TerminalArgs};
use crate::descriptor::{
    CallContext, Effect, HandlerFuture, Owner, RegisteredTool, ResourceClaims, ToolDescriptor,
    ToolFailure, ToolHandler,
};
use crate::environments::{Environments, part};
use crate::result::{Status, ToolResult};

/// The toolset of `terminal` (and of `process`, also its own toolset).
pub const TOOLSET: &str = "terminal";

/// The exclusive resource both tools hold: one session shell.
const SHELL: &str = "terminal";

struct TerminalHandler(Arc<Environments>);
struct ProcessHandler(Arc<Environments>);

/// `terminal` and `process`.
#[must_use]
pub fn tools(environments: &Arc<Environments>) -> Vec<RegisteredTool> {
    vec![
        RegisteredTool::new(
            terminal_descriptor(),
            Owner::Builtin,
            Arc::new(TerminalHandler(Arc::clone(environments))),
        ),
        RegisteredTool::new(
            process_descriptor(),
            Owner::Builtin,
            Arc::new(ProcessHandler(Arc::clone(environments))),
        ),
    ]
}

fn terminal_of(
    table: &Environments,
    context: &CallContext,
) -> Result<Arc<LocalTerminal>, ToolFailure> {
    part(table, context, "terminal", |env| env.terminal.clone())
}

impl ToolHandler for TerminalHandler {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let terminal = terminal_of(&self.0, context)?;
            let args: TerminalArgs = serde_json::from_value(arguments)
                .map_err(|err| ToolFailure::new("invalid_input", err.to_string()))?;
            Ok(to_tool_result(&terminal.run(args).await))
        })
    }
}

impl ToolHandler for ProcessHandler {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let terminal = terminal_of(&self.0, context)?;
            let args: ProcessArgs = serde_json::from_value(arguments)
                .map_err(|err| ToolFailure::new("invalid_input", err.to_string()))?;
            let reply = terminal.processes().handle(&args).await;
            let failed = reply.get("error").is_some_and(|e| !e.is_null());
            let status = if failed { Status::Error } else { Status::Ok };
            Ok(ToolResult::text(status, reply.to_string()))
        })
    }
}

/// A command that ran is `ok` whatever its exit code (the model reads
/// `exit_code`); a refusal before running is `error`.
fn to_tool_result(result: &TerminalResult) -> ToolResult {
    let status = match result.status {
        TerminalStatus::Ok | TerminalStatus::Started => Status::Ok,
        TerminalStatus::Error if result.error.is_none() => Status::Ok,
        TerminalStatus::Error => Status::Error,
        TerminalStatus::Timeout => Status::Timeout,
        TerminalStatus::Degraded => Status::Degraded,
    };
    ToolResult::text(status, result.to_json().to_string())
}

fn terminal_descriptor() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "terminal",
        TOOLSET,
        "Run a shell command in the session's sandboxed shell and get its output, exit \
         code and the working directory. The working directory and exported variables \
         persist between calls. Long-running work (servers, watchers) must use \
         background=true; foreground calls stop at the timeout. Output is truncated to \
         its head and tail when long, with the full output saved to a file.",
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The shell command line."},
                "background": {"type": "boolean", "description": "Run as a tracked background process."},
                "timeout": {"type": "number", "description": "Seconds before a foreground command is stopped."},
                "workdir": {"type": "string", "description": "Directory for this call only."},
                "pty": {"type": "boolean", "description": "Pseudo-terminal (background only)."},
                "notify_on_complete": {"type": "boolean", "description": "Notify when the background process exits."},
                "watch_patterns": {"type": "array", "items": {"type": "string"}, "description": "Output patterns to report (background only)."},
                "heartbeat": {"type": "number", "description": "Seconds between progress notes (background only)."}
            },
            "required": ["command"],
            "additionalProperties": false
        }),
    );
    shell_traits(&mut d);
    d
}

fn process_descriptor() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "process",
        "process",
        "Manage background processes started by the terminal tool: list them, poll or \
         read their output (log), wait for one to exit, write or submit input, close \
         its input, or kill it.",
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ACTIONS},
                "process_id": {"type": "string", "description": "The process id or a unique prefix."},
                "data": {"type": "string", "description": "Input for write and submit."},
                "timeout": {"type": "number", "description": "Seconds wait waits at most."}
            },
            "required": ["action"],
            "additionalProperties": false
        }),
    );
    shell_traits(&mut d);
    d
}

fn shell_traits(descriptor: &mut ToolDescriptor) {
    descriptor.capabilities = vec![Capability::parse("process.spawn:*").expect("valid capability")];
    descriptor.effect = Effect::External;
    descriptor.claims = Some(ResourceClaims::exclusive(SHELL));
}
