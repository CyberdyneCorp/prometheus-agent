//! Tools from `tool`-world plugins (spec: plugin-system, Plugin tool
//! registration).
//!
//! Each plugin's tools join the profile's tool registry under a toolset
//! named after the plugin, owned by it, with JSON-Schema parameters. The
//! registry enforces the naming rules: a core tool's name is refused unless
//! the plugin holds `tools.override:<name>` (its `tools_override` grant),
//! and a name another plugin already registered is refused naming both.
//! Sessions freeze their tool surface at start, so a plugin tool joins only
//! sessions started after it was registered.

use std::sync::Arc;

use pmt_core::capability::CapabilitySet;
use pmt_core::error::{Error, codes};
use pmt_tools::descriptor::{
    CallContext, HandlerFuture, Owner, RegisteredTool, ToolDescriptor, ToolFailure, ToolHandler,
};
use pmt_tools::{Status, ToolRegistry, ToolResult, Toolset};
use serde_json::Value;

use crate::host::{CallInfo, Instance};

/// One plugin's tool calls run on a blocking thread under the call bounds.
struct PluginTool {
    instance: Arc<Instance>,
    name: String,
    /// The surface and working directory of the sessions the tool serves;
    /// each call adds its session id.
    base: CallInfo,
}

impl ToolHandler for PluginTool {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        let instance = Arc::clone(&self.instance);
        let name = self.name.clone();
        let call = CallInfo {
            session_id: Some(context.session_id.clone()),
            ..self.base.clone()
        };
        Box::pin(async move {
            let arguments = arguments.to_string();
            let outcome =
                tokio::task::spawn_blocking(move || instance.call_tool(&call, &name, &arguments))
                    .await
                    .map_err(|e| ToolFailure::new("panic", e.to_string()))?;
            Ok(match outcome {
                Ok(Ok(text)) => ToolResult::ok(text),
                Ok(Err(text)) => ToolResult::error(text),
                Err(err) if err.message().ends_with("exceeded call timeout") => {
                    ToolResult::text(Status::Timeout, err.message())
                }
                Err(err) => ToolResult::error(err.message()),
            })
        })
    }
}

/// Registers `instance`'s tools under the toolset `<plugin id>`. `grants`
/// is the plugin's current grant (`tools.override:<tool>` lets it replace
/// a core tool); `base` is the surface and working directory its calls
/// report through `session.current` (the session id is added per call).
/// Returns the registered names; every refusal is also kept
/// in the registry's rejections.
///
/// # Errors
///
/// `plugins.call` when the plugin's `tools()` fails; a refused toolset.
/// A refused tool is skipped (and reported by the registry), not an error.
pub fn register(
    registry: &mut ToolRegistry,
    instance: &Arc<Instance>,
    grants: &CapabilitySet,
    base: &CallInfo,
) -> Result<Vec<String>, Error> {
    let id = instance.id().to_owned();
    let owner = Owner::Plugin(id.clone());
    let specs = instance.tools(base)?;
    let mut toolset = Toolset::new(&id, &format!("Tools of plugin '{id}'"), &[]);
    toolset.owner = owner.clone();
    registry.register_toolsets(&owner, vec![toolset])?;
    let mut registered = Vec::new();
    for spec in specs {
        let parameters: Value = serde_json::from_str(&spec.parameters).map_err(|e| {
            Error::new(
                &codes::PLUGINS_LOAD,
                format!(
                    "plugin '{id}' tool '{}': parameters are not JSON: {e}",
                    spec.name
                ),
            )
        })?;
        let descriptor = ToolDescriptor::new(&spec.name, &id, spec.description, parameters);
        let handler = Arc::new(PluginTool {
            instance: Arc::clone(instance),
            name: spec.name.clone(),
            base: base.clone(),
        });
        if registry
            .register(
                RegisteredTool::new(descriptor, owner.clone(), handler),
                grants,
            )
            .is_ok()
        {
            registered.push(spec.name);
        }
    }
    Ok(registered)
}
