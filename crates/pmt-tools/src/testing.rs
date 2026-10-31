//! Test fixtures shared by the tool-system unit tests.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::descriptor::{
    CallContext, HandlerFuture, Owner, RegisteredTool, ToolDescriptor, ToolFailure, ToolHandler,
};
use crate::result::ToolResult;

/// A handler running a plain async-free closure.
pub(crate) struct FnHandler<F>(pub(crate) F);

impl<F> ToolHandler for FnHandler<F>
where
    F: Fn(&CallContext, Value) -> Result<ToolResult, ToolFailure> + Send + Sync,
{
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        let outcome = (self.0)(context, arguments);
        Box::pin(async move { outcome })
    }
}

/// A handler answering `ok: <name>`.
pub(crate) fn echo(name: &str) -> Arc<dyn ToolHandler> {
    let name = name.to_owned();
    Arc::new(FnHandler(move |_: &CallContext, _| {
        Ok(ToolResult::ok(format!("ok: {name}")))
    }))
}

/// `{"type": "object", "properties": {"path": {"type": "string"}}}`.
pub(crate) fn path_schema() -> Value {
    json!({"type": "object", "properties": {"path": {"type": "string"}}})
}

/// A tool of `owner` in `toolset` answering `ok: <name>`.
pub(crate) fn tool(name: &str, toolset: &str, owner: Owner) -> RegisteredTool {
    let descriptor = ToolDescriptor::new(name, toolset, format!("The {name} tool."), path_schema());
    RegisteredTool::new(descriptor, owner, echo(name))
}

/// A built-in tool in `toolset`.
pub(crate) fn builtin(name: &str, toolset: &str) -> RegisteredTool {
    tool(name, toolset, Owner::Builtin)
}
