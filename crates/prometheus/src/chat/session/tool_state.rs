//! A running session's tool array and the one path that rebuilds it
//! (spec: tool-system, Tool surface frozen per session; MCP tool list
//! changes respect the frozen surface).
//!
//! The array is built once at session start. `/tools ... --now` and
//! `/plugins|/mcp reload --now` rebuild it on request; a compaction
//! commit rebuilds it on its own when the profile's registry moved on
//! since (an MCP server's tool list changed, a server or plugin set was
//! reloaded), because the prompt cache breaks there anyway. Between those
//! points the session keeps sending the array it froze.

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
    cache: Mutex<PluginTools>,
}

impl Registries {
    pub(super) fn new(
        scope: ProfileScope,
        extensions: SharedScope,
        store: Arc<Store>,
        cwd: PathBuf,
        built: PluginTools,
    ) -> Self {
        Self {
            scope,
            extensions,
            store,
            cwd,
            cache: Mutex::new(built),
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
        let plugins = crate::plugins_host::for_session(&scope, &self.cwd);
        let mcp = crate::mcp_host::manager(&scope);
        let mut cache = lock(&self.cache);
        if !cache.is_of(&plugins, &mcp) {
            let calls = session_calls(&self.store, &self.cwd);
            *cache = PluginTools::build(&self.scope, &plugins, &mcp, &calls)?;
        }
        Ok(cache.parts())
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
}

impl ToolState {
    /// The state of a session whose `tools` were built from `registry`,
    /// `selection` and `route`.
    pub(super) fn new(
        registries: Arc<Registries>,
        inputs: Inputs,
        tools: Arc<SwappableTools>,
        (registry, selection, route): (Arc<ToolRegistry>, Selection, RouteTraits),
    ) -> Self {
        Self {
            registries,
            inputs,
            tools,
            registry: Mutex::new(registry),
            selection: Mutex::new(selection),
            route: Mutex::new(route),
        }
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
        let route = tools::route_traits(self.inputs.model.as_ref()).await;
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
        let (registry, _) = self.registries.current()?;
        if Arc::ptr_eq(&registry, &self.registry()) {
            return Ok(false);
        }
        let selection = carried(&registry, &self.selection())?;
        self.rebuild(registry, selection).await?;
        Ok(true)
    }
}

impl CompactionBoundary for ToolState {
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
/// registered: a removed MCP server's toolset is gone) minus the tools
/// the session disabled.
fn carried(registry: &ToolRegistry, old: &Selection) -> Result<Selection, Error> {
    let kept: Vec<String> = old
        .toolsets
        .iter()
        .filter(|name| registry.canonical_toolset(name).is_some())
        .cloned()
        .collect();
    let mut selection = Selection::resolve(registry, &kept, &[])?;
    selection.tools.retain(|t| !old.disabled.contains(t));
    selection.disabled.extend(old.disabled.iter().cloned());
    selection.posture = old.posture;
    Ok(selection)
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
        let carried = carried(&registry, &old).unwrap();
        assert_eq!(carried.toolsets, ["core"]);
        assert!(!carried.contains(&off));
        assert!(carried.disabled.contains(&off));
        assert_eq!(carried.tools, old.tools);
    }
}
