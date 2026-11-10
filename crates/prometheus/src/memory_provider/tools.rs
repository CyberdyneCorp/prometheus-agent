//! The provider's tools (spec: memory, Provider tools and prompt block
//! gating): they join the profile's tool registry in the `memory` toolset,
//! so a session offers them exactly when it enables `memory` (directly or
//! through a composite), and its tool surface fixes them at session start.
//! A name a core tool or another registration already holds is refused by
//! the registry, which names both owners and keeps the refusal for
//! `prometheus memory status`. A call reaches the calling session's own
//! provider (redacted, profile-checked).

use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_core::capability::CapabilitySet;
use pmt_tools::descriptor::{
    CallContext, HandlerFuture, Owner, RegisteredTool, ToolDescriptor, ToolHandler,
};
use pmt_tools::{ToolRegistry, ToolResult};
use serde_json::Value;

use pmt_plugins::host::CallInfo;

use super::active;
use crate::plugins_host::ProfilePlugins;

/// The toolset provider tools join.
pub const TOOLSET: &str = "memory";

struct ProviderTool {
    name: String,
    profile: String,
}

impl ToolHandler for ProviderTool {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let Some(provider) = active(&context.session_id) else {
                return Ok(ToolResult::error(
                    "the memory provider is not running in this session",
                ));
            };
            Ok(
                match provider
                    .tool_call(&self.profile, &self.name, &arguments.to_string())
                    .await
                {
                    Ok(text) => ToolResult::ok(text),
                    Err(text) => ToolResult::error(text),
                },
            )
        })
    }
}

/// Registers the configured provider's tools in `registry`; the names
/// registered.
pub fn register(
    scope: &ProfileScope,
    plugins: &ProfilePlugins,
    registry: &mut ToolRegistry,
) -> Vec<String> {
    let Ok(Some(backend)) = super::configured(scope, plugins, &CallInfo::default()) else {
        return Vec::new();
    };
    let owner = Owner::Plugin(backend.id().to_owned());
    let mut registered = Vec::new();
    for spec in backend.tool_schemas() {
        let parameters = serde_json::from_str(&spec.parameters)
            .unwrap_or_else(|_| serde_json::json!({"type": "object"}));
        let descriptor = ToolDescriptor::new(&spec.name, TOOLSET, spec.description, parameters);
        let handler = Arc::new(ProviderTool {
            name: spec.name.clone(),
            profile: scope.name().as_str().to_owned(),
        });
        let tool = RegisteredTool::new(descriptor, owner.clone(), handler);
        if registry.register(tool, &CapabilitySet::default()).is_ok() {
            registered.push(spec.name);
        }
    }
    registered
}

/// The refused registrations of the configured provider's tools (`memory
/// status`).
#[must_use]
pub fn collisions(scope: &ProfileScope, registry: &ToolRegistry) -> Vec<String> {
    let owner = Owner::Plugin(scope.config().memory.provider.clone());
    registry
        .rejections()
        .iter()
        .filter(|r| r.owner == owner)
        .map(|r| r.message.clone())
        .collect()
}
