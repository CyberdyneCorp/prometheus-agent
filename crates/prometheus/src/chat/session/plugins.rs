//! What a session takes from the profile's plugins (spec: plugin-system,
//! Plugin tool registration; Plugin system-prompt sections are bounded and
//! frozen; Plugin LLM access gate; Hot reload respects the prompt-cache
//! invariant).
//!
//! Tools and system-prompt sections are read once, when the session
//! starts, from the plugin set current then; a later reload reaches them
//! only through `/plugins reload --now`, which rebuilds the session's
//! tools and prompt (one recorded `prompt.rebuilt`, reason `plugins
//! --now`). Plugin model calls made for the session run on its route.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use pmt_agent::prompt::frozen::Rebuild;
use pmt_agent::turn::model::{ModelPort, NoDeltas};
use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_plugins::host::CallInfo;
use pmt_plugins::host::ports::{LlmCall, PortFuture};
use pmt_providers::cost::CallContext;
use pmt_providers::request::{Message, Request, Role, SystemBlock};
use pmt_providers::resolve::Target;
use pmt_tools::ToolRegistry;

use super::model::{PortMaker, SwitchableModel};
use super::{Factory, Live, tools};
use crate::plugins_host::ProfilePlugins;
use crate::plugins_host::llm::{self, SessionModel};

/// Locks a session-owned mutex (a poisoned one still holds valid data).
pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The tool registry of one plugin set: the core tools plus the plugins'
/// tools, and the plugin toolsets holding them.
pub(super) struct PluginTools {
    generation: u64,
    registry: Arc<ToolRegistry>,
    toolsets: Vec<String>,
}

impl PluginTools {
    /// The registry of `plugins`' current set.
    pub(super) fn build(
        scope: &ProfileScope,
        plugins: &ProfilePlugins,
        cwd: &Path,
    ) -> Result<Self, Error> {
        let generation = plugins.generation();
        let mut registry = crate::tools_cmd::registry(scope)?;
        let toolsets = plugins.current().register_tools(
            &mut registry,
            &CallInfo {
                session_id: None,
                surface: "cli".to_owned(),
                cwd: Some(cwd.to_string_lossy().into_owned()),
            },
        );
        Ok(Self {
            generation,
            registry: Arc::new(registry),
            toolsets,
        })
    }
}

impl Factory {
    /// The tool registry and plugin toolsets for a session starting now:
    /// the profile's plugin set is refreshed first (a change since the
    /// last session loads anew) and the registry rebuilt when the set
    /// changed.
    pub(super) fn session_registry(&self) -> Result<(Arc<ToolRegistry>, Vec<String>), Error> {
        let plugins = crate::plugins_host::for_session(&self.scope, &self.options.cwd);
        let mut tools = lock(&self.tools);
        if tools.generation != plugins.generation() {
            *tools = PluginTools::build(&self.scope, &plugins, &self.options.cwd)?;
        }
        Ok((Arc::clone(&tools.registry), tools.toolsets.clone()))
    }

    /// The plugin system-prompt sections of a session starting now.
    pub(super) async fn plugin_sections(&self, id: SessionId) -> Vec<String> {
        let loaded = self.plugins.current();
        let call = self.plugin_call(id);
        tokio::task::spawn_blocking(move || loaded.contributions.render_sections(&call))
            .await
            .unwrap_or_default()
    }

    /// Makes the session's route answer its plugins' model calls.
    pub(super) fn plugin_llm(
        &self,
        id: SessionId,
        model: &Arc<SwitchableModel>,
    ) -> llm::Registration {
        let session = PluginLlm {
            session: id,
            model: Arc::clone(model),
            maker: self.models.maker(),
        };
        llm::register(&id.to_string(), Arc::new(session))
    }

    /// `/plugins reload --now`: the session's tools and system-prompt
    /// sections come from the current plugin set from the next request;
    /// the prompt rebuild is recorded with reason `plugins --now`.
    ///
    /// # Errors
    ///
    /// Registry and selection errors; store errors recording the surface.
    pub(crate) async fn apply_plugins_now(&self, live: &Live) -> Result<(), Error> {
        let (registry, toolsets) = self.session_registry()?;
        let config = self.scope.config();
        let selection = tools::selection(&registry, config, self.options.interactive, &toolsets)?;
        *lock(&live.registry) = registry;
        self.rebuild_tools(live, &selection).await?;
        live.set_selection(selection);
        *lock(&live.plugin_sections) = self.plugin_sections(live.id).await;
        live.agent
            .rebuild_prompt(Rebuild::Now("plugins".to_owned()));
        Ok(())
    }
}

/// A session's model as its plugins call it.
struct PluginLlm {
    session: SessionId,
    model: Arc<SwitchableModel>,
    maker: PortMaker,
}

impl PluginLlm {
    async fn run(&self, call: LlmCall) -> Result<String, String> {
        let request = request(&call);
        let context = CallContext::new(self.session, &format!("plugin.{}", call.plugin));
        let reply = if call.model.is_some() || call.provider.is_some() {
            let current = self.model.target();
            let target = Target {
                provider: call.provider.clone().unwrap_or(current.provider),
                model: call.model.clone().or(current.model),
                base_url: None,
                credential: None,
            };
            let port = self
                .maker
                .port(&target)
                .await
                .map_err(|e| e.message().to_owned())?;
            port.call(&request, &context, &NoDeltas).await
        } else {
            self.model.call(&request, &context, &NoDeltas).await
        };
        let text = reply.map_err(|f| f.error.message().to_owned())?.text;
        if call.schema.is_some() {
            serde_json::from_str::<serde_json::Value>(text.trim())
                .map_err(|e| format!("the model did not answer JSON: {e}"))?;
        }
        Ok(text)
    }
}

impl SessionModel for PluginLlm {
    fn complete(&self, call: LlmCall) -> PortFuture<'_, Result<String, String>> {
        Box::pin(self.run(call))
    }
}

/// The provider request of a plugin call: `system` messages become
/// system blocks, and a structured call asks for JSON matching its schema.
fn request(call: &LlmCall) -> Request {
    let mut system = Vec::new();
    let mut messages = Vec::new();
    for (role, content) in &call.messages {
        match role.as_str() {
            "system" => system.push(block(content.clone())),
            "assistant" => messages.push(Message::text(Role::Assistant, content.clone())),
            _ => messages.push(Message::text(Role::User, content.clone())),
        }
    }
    if let Some(schema) = &call.schema {
        system.push(block(format!(
            "Answer with one JSON value only, matching this JSON Schema:\n{schema}"
        )));
    }
    Request {
        system,
        messages,
        max_output_tokens: call.max_tokens.map(u64::from),
        ..Request::default()
    }
}

fn block(text: String) -> SystemBlock {
    SystemBlock {
        text,
        cache_breakpoint: false,
    }
}
