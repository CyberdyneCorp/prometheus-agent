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
use pmt_config::{Launch, Overrides, ProfileScope};
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_plugins::host::CallInfo;
use pmt_plugins::host::ports::{LlmCall, PortFuture};
use pmt_plugins::tools::SessionCalls;
use pmt_providers::cost::CallContext;
use pmt_providers::request::{Message, Request, Role, SystemBlock};
use pmt_providers::resolve::Target;
use pmt_store::Store;
use pmt_tools::ToolRegistry;

use super::model::{PortMaker, SwitchableModel};
use super::{Factory, Live, tools};
use crate::plugins_host::ProfilePlugins;
use crate::plugins_host::llm::{self, SessionModel};

/// Locks a session-owned mutex (a poisoned one still holds valid data).
pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The tool registry of one plugin set and one MCP server set: the core
/// tools plus the plugins' and the MCP servers' tools, and the toolsets
/// holding those.
pub(super) struct PluginTools {
    generation: (u64, u64),
    registry: Arc<ToolRegistry>,
    toolsets: Vec<String>,
}

impl PluginTools {
    /// The registry of `plugins`' current set and of the tools `mcp`'s
    /// servers listed at their last start. `calls` resolves what each
    /// plugin tool call reports as its session.
    pub(super) fn build(
        scope: &ProfileScope,
        plugins: &ProfilePlugins,
        mcp: &pmt_mcp::Manager,
        calls: &SessionCalls,
    ) -> Result<Self, Error> {
        let generation = (plugins.generation(), mcp.generation());
        let (registry, toolsets) = crate::tools_cmd::extended_registry(scope, plugins, mcp, calls)?;
        Ok(Self {
            generation,
            registry: Arc::new(registry),
            toolsets,
        })
    }

    /// Whether this was built from `plugins`' and `mcp`'s current sets.
    pub(super) fn is_of(&self, plugins: &ProfilePlugins, mcp: &pmt_mcp::Manager) -> bool {
        self.generation == (plugins.generation(), mcp.generation())
    }

    /// The registry and the plugin and MCP toolsets.
    pub(super) fn parts(&self) -> (Arc<ToolRegistry>, Vec<String>) {
        (Arc::clone(&self.registry), self.toolsets.clone())
    }
}

/// The surface that created session `id` (its stored source: `cli`,
/// `tui`, `gateway`, `cron`...); `cli` when the store does not know it.
pub(super) fn stored_surface(store: &Store, id: SessionId) -> String {
    store
        .session(id)
        .map_or_else(|_| "cli".to_owned(), |s| s.source.as_str().to_owned())
}

/// What a plugin tool call reports as its session: the calling session's
/// stored surface and the factory's working directory. The registry is
/// shared by every session of the factory (in the daemon: gateway, TUI and
/// attached CLI sessions alike), so the surface is looked up per call.
pub(super) fn session_calls(store: &Arc<Store>, cwd: &Path) -> SessionCalls {
    let (store, cwd) = (Arc::clone(store), cwd.to_path_buf());
    Arc::new(move |session: &str| {
        let surface = SessionId::parse(session)
            .map_or_else(|_| "cli".to_owned(), |id| stored_surface(&store, id));
        CallInfo {
            session_id: (!session.is_empty()).then(|| session.to_owned()),
            surface,
            cwd: Some(cwd.to_string_lossy().into_owned()),
        }
    })
}

impl Factory {
    /// The tool registry and plugin and MCP toolsets for a session
    /// starting now ([`super::tool_state::Registries::current`]).
    pub(super) fn session_registry(&self) -> Result<(Arc<ToolRegistry>, Vec<String>), Error> {
        self.registries.current()
    }

    /// Starts the profile's MCP servers a session starting now needs
    /// ([`crate::mcp_host::discover`]).
    pub(super) async fn discover_mcp(&self) {
        let scope = self.plugin_scope.get();
        let one_shot = self.options.source_tag.as_deref() == Some(pmt_store::ONESHOT_TAG);
        crate::mcp_host::discover(&scope, one_shot).await;
    }

    /// Makes `/plugins reload` re-read the profile's `config.toml` (with
    /// the `--set` list `set` the session opened its scope with) so a
    /// `plugins enable`/`disable` run since the session started counts. A
    /// one-process session only: the daemon re-reads its config itself.
    #[must_use]
    pub fn with_config_reread(mut self, set: Vec<String>) -> Self {
        self.reread = Some(set);
        self
    }

    /// The configuration `/plugins reload` loads the profile's plugins
    /// from: `current` (the daemon's), or in a one-process session
    /// `config.toml` read again through the session's own overrides (and
    /// `--ignore-user-config` / `--safe-mode`), which later sessions of
    /// this process then load their plugins from too.
    ///
    /// # Errors
    ///
    /// The re-read configuration's errors (the plugin set stays as it is).
    pub(crate) fn plugins_reload_scope(
        &self,
        current: &ProfileScope,
    ) -> Result<ProfileScope, Error> {
        let Some(set) = &self.reread else {
            return Ok(current.clone());
        };
        let overrides = Overrides::parse(set)?;
        let dir = self.scope.profile().clone();
        let launch = Launch::Profile(&self.launch);
        let fresh = if self.options.ignore_user_config {
            ProfileScope::open_defaults(dir, &overrides, launch)
        } else {
            ProfileScope::open(dir, &overrides, launch)
        }?;
        self.plugin_scope.replace(fresh.clone());
        Ok(fresh)
    }

    /// The configuration the profile's plugins and MCP servers are loaded
    /// from now: `current` (the daemon's), or in a one-process session the
    /// one the last `/plugins reload` or `/mcp reload` read.
    pub(crate) fn extensions_scope(&self, current: &ProfileScope) -> ProfileScope {
        if self.reread.is_some() {
            self.plugin_scope.get()
        } else {
            current.clone()
        }
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
        llm::register(&id.to_string(), self.session_llm(id, model))
    }

    /// The session's model as plugins and MCP sampling call it.
    pub(super) fn session_llm(
        &self,
        id: SessionId,
        model: &Arc<SwitchableModel>,
    ) -> Arc<PluginLlm> {
        Arc::new(PluginLlm {
            session: id,
            model: Arc::clone(model),
            maker: self.models.maker(),
        })
    }

    /// `/plugins reload --now`: the session's tools and system-prompt
    /// sections come from the current plugin set from the next request;
    /// the prompt rebuild is recorded with reason `plugins --now`.
    ///
    /// # Errors
    ///
    /// Registry and selection errors; store errors recording the surface.
    pub(crate) async fn apply_plugins_now(&self, live: &Live) -> Result<(), Error> {
        self.apply_tools_now(live, "plugins").await
    }

    /// `/mcp reload --now` (`reason` `mcp`) and `/plugins reload --now`
    /// (`plugins`): the session's tools (and plugin system-prompt
    /// sections) come from the current plugin and MCP server sets from the
    /// next request; the prompt rebuild is recorded with `reason --now`.
    ///
    /// # Errors
    ///
    /// Registry and selection errors; store errors recording the surface.
    pub(crate) async fn apply_tools_now(&self, live: &Live, reason: &str) -> Result<(), Error> {
        let (registry, toolsets) = self.session_registry()?;
        let config = self.scope.config();
        let selection = tools::selection(
            &registry,
            config,
            self.options.interactive,
            &toolsets,
            &live.session_surface,
        )?;
        live.tool_state.rebuild(registry, selection).await?;
        *lock(&live.plugin_sections) = self.plugin_sections(live.id).await;
        live.agent.rebuild_prompt(Rebuild::Now(reason.to_owned()));
        Ok(())
    }
}

/// A session's model as its plugins (and MCP servers' sampling) call it.
pub(super) struct PluginLlm {
    pub(super) session: SessionId,
    pub(super) model: Arc<SwitchableModel>,
    pub(super) maker: PortMaker,
}

impl PluginLlm {
    async fn run(&self, call: LlmCall) -> Result<String, String> {
        let purpose = format!("plugin.{}", call.plugin);
        self.run_as(&purpose, call).await
    }

    /// Runs `call`, charged to the session's ledger as `purpose`.
    pub(super) async fn run_as(&self, purpose: &str, call: LlmCall) -> Result<String, String> {
        let request = request(&call);
        let context = CallContext::new(self.session, purpose);
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
