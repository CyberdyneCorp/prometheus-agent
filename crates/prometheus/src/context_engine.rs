//! Plugin context engines (spec: context-compression, Context engine
//! selection; memory, Context engine selection).
//!
//! `context.engine` names `builtin` or a loaded plugin implementing the
//! `context-engine` world (named by its plugin id); an unknown name fails
//! with exit 78 and a plugin engine is never used unless the key names it.
//! At session start the engine is initialized for the session; one that
//! fails leaves the session on the built-in engine with one warning. The
//! engine compacts through the host, which validates its output before
//! committing, and may choose the rows of each outgoing request; it never
//! rewrites stored history. Its tools join the `context_engine` toolset.

use std::sync::Arc;

use pmt_agent::compress::engine::{self, ContextEngine, Engine, Output};
use pmt_agent::turn::model::BoxFuture;
use pmt_config::ProfileScope;
use pmt_config::schema::ContextConfig;
use pmt_core::error::Error;
use pmt_core::id::{MessageId, SessionId};
use pmt_plugins::host::{CallInfo, Instance};
use pmt_store::{Message as Row, NewMessage, Role};
use serde_json::{Value, json};

use crate::plugins_host::ProfilePlugins;

/// The world an engine plugin implements.
pub const WORLD: &str = "context-engine";

/// The engine names `plugins` register (their ids).
#[must_use]
pub fn names(plugins: &ProfilePlugins) -> Vec<String> {
    plugins
        .current()
        .plugins
        .iter()
        .filter(|p| p.instance.implements(WORLD))
        .map(|p| p.manifest.id.clone())
        .collect()
}

/// The engine `config` selects among the built-in one and `plugins`'.
///
/// # Errors
///
/// `unknown context engine '<name>'` (exit 78).
pub fn select(config: &ContextConfig, plugins: &ProfilePlugins) -> Result<Engine, Error> {
    engine::select(config, &names(plugins))
}

/// A plugin engine behind [`ContextEngine`].
struct WasmEngine {
    instance: Arc<Instance>,
    call: CallInfo,
}

fn messages(rows: &[Row]) -> String {
    Value::Array(
        rows.iter()
            .map(|r| json!({"id": r.id.to_string(), "role": r.role.as_str(), "content": r.content}))
            .collect(),
    )
    .to_string()
}

fn role(name: &str) -> Role {
    match name {
        "user" => Role::User,
        "tool" => Role::Tool,
        "system" => Role::System,
        _ => Role::Assistant,
    }
}

/// The engine's compacted transcript as host [`Output`]s.
fn outputs(text: &str) -> Result<Vec<Output>, String> {
    let items: Vec<Value> =
        serde_json::from_str(text).map_err(|e| format!("the engine returned no JSON list: {e}"))?;
    items
        .iter()
        .map(|item| {
            if let Some(id) = item["keep"].as_str() {
                return id
                    .parse()
                    .map(Output::Kept)
                    .map_err(|_| format!("the engine kept an invalid message id '{id}'"));
            }
            let content = item["content"].as_str().unwrap_or_default();
            let row = NewMessage::new(role(item["role"].as_str().unwrap_or("assistant")), content);
            Ok(Output::New(Box::new(row)))
        })
        .collect()
}

impl ContextEngine for WasmEngine {
    fn compact<'a>(&'a self, rows: &'a [Row]) -> BoxFuture<'a, Result<Vec<Output>, String>> {
        Box::pin(async move {
            let (instance, call, text) = (
                Arc::clone(&self.instance),
                self.call.clone(),
                messages(rows),
            );
            let compacted =
                pmt_core::spawn::blocking(move || instance.engine_compact(&call, &text))
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| e.message().to_owned())??;
            outputs(&compacted)
        })
    }

    fn select<'a>(
        &'a self,
        rows: &'a [Row],
        budget: u64,
    ) -> BoxFuture<'a, Result<Option<Vec<MessageId>>, String>> {
        Box::pin(async move {
            let (instance, call, text) = (
                Arc::clone(&self.instance),
                self.call.clone(),
                messages(rows),
            );
            let chosen =
                pmt_core::spawn::blocking(move || instance.engine_select(&call, &text, budget))
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| e.message().to_owned())??;
            Ok(chosen.map(|ids| ids.iter().filter_map(|id| id.parse().ok()).collect()))
        })
    }
}

/// The plugin engine of session `id`, initialized; `None` for the built-in
/// engine, or (with one warning) when the plugin engine failed to start.
#[must_use]
pub fn for_session(
    scope: &ProfileScope,
    plugins: &ProfilePlugins,
    id: SessionId,
    call: &CallInfo,
) -> Option<Arc<dyn ContextEngine>> {
    let Ok(Engine::Plugin(name)) = select(&scope.config().context, plugins) else {
        return None;
    };
    let loaded = plugins.current();
    let plugin = loaded.plugins.iter().find(|p| p.manifest.id == name)?;
    let started = plugin
        .instance
        .engine_initialize(call, &id.to_string(), scope.name().as_str())
        .map_err(|e| e.message().to_owned())
        .and_then(|r| r);
    if let Err(err) = started {
        tracing::warn!(engine = %name, "context engine failed to start ({err}); this session uses the built-in engine");
        return None;
    }
    Some(Arc::new(WasmEngine {
        instance: Arc::clone(&plugin.instance),
        call: call.clone(),
    }))
}

/// Registers the selected engine's tools under `context_engine`.
pub fn register_tools(
    scope: &ProfileScope,
    plugins: &ProfilePlugins,
    registry: &mut pmt_tools::ToolRegistry,
) {
    use pmt_tools::descriptor::{Owner, RegisteredTool, ToolDescriptor};
    let Ok(Engine::Plugin(name)) = select(&scope.config().context, plugins) else {
        return;
    };
    let loaded = plugins.current();
    let Some(plugin) = loaded.plugins.iter().find(|p| p.manifest.id == name) else {
        return;
    };
    let Ok(specs) = plugin.instance.engine_tool_schemas(&CallInfo::default()) else {
        return;
    };
    for spec in specs {
        let parameters =
            serde_json::from_str(&spec.parameters).unwrap_or_else(|_| json!({"type": "object"}));
        let descriptor =
            ToolDescriptor::new(&spec.name, "context_engine", spec.description, parameters);
        let handler = Arc::new(EngineTool {
            instance: Arc::clone(&plugin.instance),
            name: spec.name.clone(),
        });
        let tool = RegisteredTool::new(descriptor, Owner::Plugin(name.clone()), handler);
        let _ = registry.register(tool, &pmt_core::capability::CapabilitySet::default());
    }
}

struct EngineTool {
    instance: Arc<Instance>,
    name: String,
}

impl pmt_tools::descriptor::ToolHandler for EngineTool {
    fn call<'a>(
        &'a self,
        context: &'a pmt_tools::descriptor::CallContext,
        arguments: Value,
    ) -> pmt_tools::descriptor::HandlerFuture<'a> {
        Box::pin(async move {
            let (instance, name) = (Arc::clone(&self.instance), self.name.clone());
            let call = CallInfo {
                session_id: Some(context.session_id.clone()),
                ..CallInfo::default()
            };
            let outcome = pmt_core::spawn::blocking(move || {
                instance.engine_tool_call(&call, &name, &arguments.to_string())
            })
            .await
            .map_err(|e| pmt_tools::descriptor::ToolFailure::new("panic", e.to_string()))?;
            Ok(match outcome {
                Ok(Ok(text)) => pmt_tools::ToolResult::ok(text),
                Ok(Err(text)) => pmt_tools::ToolResult::error(text),
                Err(err) => pmt_tools::ToolResult::error(err.message()),
            })
        })
    }
}
