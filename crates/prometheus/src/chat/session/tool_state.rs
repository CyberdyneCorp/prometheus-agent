//! A running session's tool array and the one path that rebuilds it
//! (spec: tool-system, Tool surface frozen per session; MCP tool list
//! changes respect the frozen surface).
//!
//! The array is built once at session start. `/tools ... --now` and
//! `/plugins|/mcp reload --now` rebuild it on request; a compaction
//! commit rebuilds it on its own when the profile's registry moved on
//! since (an MCP server's tool list changed, a server or plugin set was
//! reloaded), because the prompt cache breaks there anyway. A new session
//! whose MCP servers were still connecting when it opened settles its
//! array once, before its first prompt is built ([`Deferred`]). Between
//! those points the session keeps sending the array it froze.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pmt_agent::tools::AgentTools;
use pmt_agent::turn::CompactionBoundary;
use pmt_agent::turn::model::BoxFuture;
use pmt_config::{ProfileScope, SharedScope};
use pmt_core::capability::CapabilitySet;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_core::secret::Redactor;
use pmt_store::Store;
use pmt_tools::approval::CommandGate;
use pmt_tools::dispatch::ToolHooks;
use pmt_tools::variants::RouteTraits;
use pmt_tools::{Selection, ToolRegistry};

use super::model::SwitchableModel;
use super::plugins::{PluginTools, lock, session_calls};
use super::tools::{self, SurfaceInputsOwned, SwappableTools};

/// The profile's tool registry as sessions starting now get it: the core
/// tools plus the current plugin set's and MCP servers' tools, rebuilt
/// when either set's generation moved on. Shared by the factory and every
/// session it opened.
pub(super) struct Registries {
    scope: ProfileScope,
    /// Where the plugin set and MCP servers are loaded from (the
    /// factory's, replaced by a one-process `/plugins reload`).
    extensions: SharedScope,
    store: Arc<Store>,
    cwd: PathBuf,
    /// Its sessions load the plugins `plugins dev` serves.
    dev_plugins: bool,
    cache: Mutex<PluginTools>,
    /// Per editor (ACP) session with editor MCP servers: its registry,
    /// the profile registry and binding generation it was built from.
    editors: Mutex<HashMap<SessionId, EditorRegistry>>,
}

/// A session's registry with its editor MCP servers' tools on top.
struct EditorRegistry {
    base: Arc<ToolRegistry>,
    generation: u64,
    registry: Arc<ToolRegistry>,
    toolsets: Vec<String>,
}

impl Registries {
    pub(super) fn new(
        scope: ProfileScope,
        extensions: SharedScope,
        store: Arc<Store>,
        cwd: PathBuf,
        dev_plugins: bool,
        built: PluginTools,
    ) -> Self {
        Self {
            scope,
            extensions,
            store,
            cwd,
            dev_plugins,
            cache: Mutex::new(built),
            editors: Mutex::default(),
        }
    }

    /// The registry and the plugin and MCP toolsets for a session starting
    /// now: the profile's plugin set is refreshed first (a change since the
    /// last session loads anew) and the registry rebuilt when the set or an
    /// MCP server's tool list changed. The same `Arc` comes back until
    /// then.
    ///
    /// # Errors
    ///
    /// The core registry could not be built.
    pub(super) fn current(&self) -> Result<(Arc<ToolRegistry>, Vec<String>), Error> {
        let scope = self.extensions.get();
        let plugins = crate::plugins_host::for_attended(&scope, &self.cwd, self.dev_plugins);
        let mcp = crate::mcp_host::manager(&scope);
        let mut cache = lock(&self.cache);
        if !cache.is_of(&plugins, &mcp) {
            let calls = session_calls(&self.store, &self.cwd);
            *cache = PluginTools::build(&self.scope, &plugins, &mcp, &calls)?;
        }
        Ok(cache.parts())
    }

    /// [`Self::current`] for session `session`: with its editor MCP
    /// servers' tools when an editor bound some (spec: acp-adapter, Editor
    /// MCP servers belong to the daemon session).
    ///
    /// # Errors
    ///
    /// The core registry could not be built.
    pub(super) fn current_for(
        &self,
        session: SessionId,
    ) -> Result<(Arc<ToolRegistry>, Vec<String>), Error> {
        let (base, toolsets) = self.current()?;
        let Some((editor, generation)) = crate::mcp_host::editor::manager(&session.to_string())
        else {
            lock(&self.editors).remove(&session);
            return Ok((base, toolsets));
        };
        let mut editors = lock(&self.editors);
        if let Some(built) = editors.get(&session)
            && Arc::ptr_eq(&built.base, &base)
            && built.generation == generation
        {
            return Ok((Arc::clone(&built.registry), built.toolsets.clone()));
        }
        let scope = self.extensions.get();
        let plugins = crate::plugins_host::for_attended(&scope, &self.cwd, self.dev_plugins);
        let mcp = crate::mcp_host::manager(&scope);
        let calls = session_calls(&self.store, &self.cwd);
        let (mut registry, mut toolsets) =
            crate::tools_cmd::extended_registry(&self.scope, &plugins, &mcp, &calls)?;
        toolsets.extend(editor.register(&mut registry));
        let registry = Arc::new(registry);
        editors.insert(
            session,
            EditorRegistry {
                base,
                generation,
                registry: Arc::clone(&registry),
                toolsets: toolsets.clone(),
            },
        );
        Ok((registry, toolsets))
    }
}

/// What a session's tools are built from besides the registry and the
/// selection: fixed for the session.
pub(super) struct Inputs {
    pub(super) scope: ProfileScope,
    pub(super) store: Arc<Store>,
    pub(super) session: SessionId,
    pub(super) capabilities: CapabilitySet,
    pub(super) gate: Arc<CommandGate>,
    pub(super) agent: Arc<AgentTools>,
    pub(super) redactor: Redactor,
    pub(super) hooks: Arc<dyn ToolHooks>,
    pub(super) model: Arc<SwitchableModel>,
}

impl Inputs {
    /// The surface inputs of `registry` and `selection` on `route`.
    pub(super) fn surface<'a>(
        &'a self,
        registry: &'a ToolRegistry,
        selection: &'a Selection,
        route: &'a RouteTraits,
    ) -> SurfaceInputsOwned<'a> {
        SurfaceInputsOwned {
            scope: &self.scope,
            store: &self.store,
            session: self.session,
            registry,
            selection,
            capabilities: &self.capabilities,
            gate: &self.gate,
            agent: Some(&self.agent),
            redactor: &self.redactor,
            context_window: None,
            hooks: Arc::clone(&self.hooks),
            route,
        }
    }
}

/// One session's tool array, the registry and selection it was built
/// from, and the main route its schema variants were picked for.
pub(super) struct ToolState {
    registries: Arc<Registries>,
    inputs: Inputs,
    tools: Arc<SwappableTools>,
    registry: Mutex<Arc<ToolRegistry>>,
    selection: Mutex<Selection>,
    route: Mutex<RouteTraits>,
    deferred: tokio::sync::Mutex<Option<Deferred>>,
    /// The editor MCP binding generation the tools were built with.
    editor_generation: std::sync::atomic::AtomicU64,
}

/// The MCP discovery a new session did not wait for at open (so its
/// client's prompt is not delayed), and how the session selects its
/// toolsets from a registry and the extension toolsets that opt in.
pub(super) struct Deferred {
    /// Ends once every server connected or the discovery wait ran out.
    pub(super) discovery: tokio::task::JoinHandle<()>,
    /// The session's selection over a registry.
    pub(super) select: Selector,
}

/// How a session selects its toolsets from a registry and the extension
/// toolsets that opt in.
pub(super) type Selector =
    Box<dyn Fn(&ToolRegistry, &[String]) -> Result<Selection, Error> + Send + Sync>;

impl ToolState {
    /// The state of a session whose `tools` were built from `registry`,
    /// `selection` and `route`.
    pub(super) fn new(
        registries: Arc<Registries>,
        inputs: Inputs,
        tools: Arc<SwappableTools>,
        (registry, selection, route): (Arc<ToolRegistry>, Selection, RouteTraits),
    ) -> Self {
        let editor_generation = crate::mcp_host::editor::generation(&inputs.session.to_string());
        Self {
            registries,
            inputs,
            tools,
            registry: Mutex::new(registry),
            selection: Mutex::new(selection),
            route: Mutex::new(route),
            deferred: tokio::sync::Mutex::new(None),
            editor_generation: std::sync::atomic::AtomicU64::new(editor_generation),
        }
    }

    /// At a turn start: when an editor bound other MCP servers since the
    /// tools were built, connects them and rebuilds the tools with their
    /// toolsets (the new list applies from this turn). `true` when the
    /// tools were rebuilt.
    ///
    /// # Errors
    ///
    /// Registry, selection and store errors.
    pub(super) async fn refresh_editor(&self) -> Result<bool, Error> {
        use std::sync::atomic::Ordering;
        let session = self.inputs.session.to_string();
        let now = crate::mcp_host::editor::generation(&session);
        if self.editor_generation.swap(now, Ordering::AcqRel) == now {
            return Ok(false);
        }
        crate::mcp_host::editor::connect(&session).await;
        let (registry, _) = self.registries.current_for(self.inputs.session)?;
        let editor = crate::mcp_host::editor::toolsets(&session);
        let selection = carried(&registry, &self.selection(), &editor)?;
        self.rebuild(registry, selection).await?;
        Ok(true)
    }

    /// Settles the tools once `deferred`'s discovery ends, before the
    /// first prompt ([`CompactionBoundary::settle_tools`]).
    pub(super) fn defer(&self, deferred: Deferred) {
        if let Ok(mut slot) = self.deferred.try_lock() {
            *slot = Some(deferred);
        }
    }

    /// Waits for a deferred discovery, then builds the session's tools
    /// again from the profile's registry, keeping what the session disabled
    /// or opted into meanwhile: an MCP server's tools are built only from a
    /// connected server, so the array is rebuilt even when the selection
    /// did not change. `true` when the tools were rebuilt.
    async fn settle(&self) -> Result<bool, Error> {
        // Held while waiting: a second turn waits for the same settling.
        let mut slot = self.deferred.lock().await;
        let Some(deferred) = slot.take() else {
            return Ok(false);
        };
        let _ = deferred.discovery.await;
        let (registry, toolsets) = self.registries.current_for(self.inputs.session)?;
        let old = self.selection();
        let mut selection = (deferred.select)(&registry, &toolsets)?;
        selection.tools.retain(|t| !old.disabled.contains(t));
        selection.disabled.extend(old.disabled.iter().cloned());
        let selection =
            selection.with_session_opt_ins(old.session_opt_ins.iter().map(String::as_str));
        self.rebuild(registry, selection).await?;
        Ok(true)
    }

    /// The port the agent calls the tools through.
    pub(super) fn tools(&self) -> Arc<SwappableTools> {
        Arc::clone(&self.tools)
    }

    pub(super) fn registry(&self) -> Arc<ToolRegistry> {
        Arc::clone(&lock(&self.registry))
    }

    pub(super) fn selection(&self) -> Selection {
        lock(&self.selection).clone()
    }

    pub(super) fn route(&self) -> RouteTraits {
        lock(&self.route).clone()
    }

    /// Serves tools built from `registry` and `selection` (for the
    /// session's current route) from the next request.
    ///
    /// # Errors
    ///
    /// Store errors recording the surface.
    pub(super) async fn rebuild(
        &self,
        registry: Arc<ToolRegistry>,
        selection: Selection,
    ) -> Result<(), Error> {
        let route = Box::pin(tools::session_route(
            self.inputs.model.as_ref(),
            self.inputs.session,
        ))
        .await;
        let built =
            tools::session_tools(&self.inputs.surface(&registry, &selection, &route)).await?;
        self.tools.swap(built);
        *lock(&self.registry) = registry;
        *lock(&self.selection) = selection;
        *lock(&self.route) = route;
        Ok(())
    }

    /// At a compaction boundary: rebuilds the tools from the profile's
    /// current registry when it is not the one they were built from,
    /// keeping the session's toolsets and what it disabled. `true` when
    /// the tools were rebuilt.
    ///
    /// # Errors
    ///
    /// Registry, selection and store errors.
    pub(super) async fn refresh_if_stale(&self) -> Result<bool, Error> {
        let (registry, _) = self.registries.current_for(self.inputs.session)?;
        if Arc::ptr_eq(&registry, &self.registry()) {
            return Ok(false);
        }
        let selection = carried(&registry, &self.selection(), &[])?;
        self.rebuild(registry, selection).await?;
        Ok(true)
    }
}

impl CompactionBoundary for ToolState {
    fn settle_tools(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            match self.settle().await {
                Ok(true) => tracing::info!(
                    session = %self.inputs.session,
                    "tools of extensions that finished loading joined before the first prompt"
                ),
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    session = %self.inputs.session,
                    "the tools could not be settled; keeping the ones the session opened with: {e}"
                ),
            }
        })
    }

    fn refresh_tools(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            match self.refresh_if_stale().await {
                Ok(true) => tracing::info!(
                    session = %self.inputs.session,
                    "tools rebuilt at the compaction boundary"
                ),
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    session = %self.inputs.session,
                    "the tools could not be rebuilt at the compaction boundary; keeping them: {e}"
                ),
            }
        })
    }
}

/// `old` against a newer `registry`: the same toolsets (those still
/// registered: a removed MCP server's toolset is gone) plus `added`, minus
/// the tools the session disabled.
fn carried(registry: &ToolRegistry, old: &Selection, added: &[String]) -> Result<Selection, Error> {
    let mut kept: Vec<String> = old
        .toolsets
        .iter()
        .filter(|name| registry.canonical_toolset(name).is_some())
        .cloned()
        .collect();
    for name in added {
        if !kept.contains(name) && registry.canonical_toolset(name).is_some() {
            kept.push(name.clone());
        }
    }
    let mut selection = Selection::resolve(registry, &kept, &[])?;
    selection.tools.retain(|t| !old.disabled.contains(t));
    selection.disabled.extend(old.disabled.iter().cloned());
    selection.posture = old.posture;
    Ok(selection.with_session_opt_ins(old.session_opt_ins.iter().map(String::as_str)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_carried_selection_keeps_disabled_tools_out_and_drops_gone_toolsets() {
        use pmt_config::{Home, Launch, Overrides, ProfileName};
        let root = tempfile::tempdir().unwrap();
        let home = Home::open(root.path()).unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
        let registry = crate::tools_cmd::base_registry(&scope).unwrap();
        let mut old = Selection::resolve(&registry, &["core".to_owned()], &[]).unwrap();
        let off = old.tools[0].clone();
        old.tools.retain(|t| *t != off);
        old.disabled.insert(off.clone());
        old.toolsets.push("mcp-gone".to_owned());
        let carried = carried(&registry, &old, &[]).unwrap();
        assert_eq!(carried.toolsets, ["core"]);
        assert!(!carried.contains(&off));
        assert!(carried.disabled.contains(&off));
        assert_eq!(carried.tools, old.tools);
    }
}
