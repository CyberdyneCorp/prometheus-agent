//! One plugin instance: a component linked against the host imports, its
//! store, and the per-call fuel, epoch and memory bounds.
//!
//! A trap or timeout fails the call with an error naming the plugin and
//! discards the store (a trapped component cannot be re-entered); the next
//! call instantiates afresh, while persistent state lives on disk under
//! `plugin-data/`. Three traps within 60 seconds suspend the instance
//! until it is reloaded.
//!
//! Calls into most worlds share one store and run one at a time under a
//! wall-clock deadline. Terminal-backend calls are different: an `execute`
//! may wait minutes on a remote command while `cancel` must reach the
//! plugin, and other commands keep running. Each backend call therefore
//! takes a store of its own from a small pool (so calls run side by side;
//! a backend keeps what must outlive one call in `state` or in its remote
//! service, never in guest memory) under a CPU [`Meter`]: time blocked in
//! host imports does not count against the call timeout.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use pmt_core::error::{Error, codes};
use wasmtime::component::{Component, HasSelf, InstancePre, Linker};
use wasmtime::{Store, StoreLimitsBuilder, Trap};

mod adapter;
mod backend;
mod engine;
mod memory;
mod meter;
mod router;
mod surfaces;

pub(crate) use adapter::REQUIRED_OPERATIONS;
pub use memory::MemoryWrite;
pub(crate) use meter::Meter;

use super::bindings::PluginHost;
use super::bindings::exports::prometheus::{
    context_engine, delegation_router, hook, memory_provider, platform_adapter, provider,
    terminal_backend, tool,
};
use super::imports::{CallInfo, Context, HostState};
use super::{Limits, Runtime};
use crate::worlds::WorldRef;

/// Traps within [`TRAP_WINDOW`] that suspend an instance.
const TRAPS_TO_SUSPEND: usize = 3;
const TRAP_WINDOW: Duration = Duration::from_mins(1);
/// Idle backend stores kept for reuse; more concurrent calls instantiate
/// stores that are dropped afterwards.
const POOL_IDLE: usize = 4;

/// A linked, instantiable plugin.
pub struct Instance {
    id: String,
    pre: InstancePre<HostState>,
    ctx: Arc<Context>,
    limits: Limits,
    exported: Vec<WorldRef>,
    tool: Option<tool::plugin::GuestIndices>,
    hook: Option<hook::plugin::GuestIndices>,
    extensions: Option<hook::extensions::GuestIndices>,
    provider: Option<provider::plugin::GuestIndices>,
    surfaces: Option<hook::surfaces::GuestIndices>,
    models: Option<provider::models::GuestIndices>,
    backend: Option<terminal_backend::plugin::GuestIndices>,
    router: Option<delegation_router::plugin::GuestIndices>,
    memory: Option<memory_provider::plugin::GuestIndices>,
    memory_hooks: Option<memory_provider::hooks::GuestIndices>,
    engine: Option<context_engine::plugin::GuestIndices>,
    adapter: Option<platform_adapter::plugin::GuestIndices>,
    /// `tool.tools()`, read once: a session starting while the instance
    /// is busy (a plugin call waiting on a subagent it launched) must not
    /// wait for it.
    tool_specs: Mutex<Option<Vec<tool::plugin::ToolSpec>>>,
    live: Mutex<Option<Live>>,
    /// Idle stores for terminal-backend calls.
    pool: Mutex<Vec<Live>>,
    traps: Mutex<Traps>,
}

impl std::fmt::Debug for Instance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Instance")
            .field("id", &self.id)
            .field("exported", &self.exported)
            .finish_non_exhaustive()
    }
}

struct Live {
    store: Store<HostState>,
    instance: wasmtime::component::Instance,
}

#[derive(Default)]
struct Traps {
    recent: VecDeque<Instant>,
    suspended: bool,
}

impl Instance {
    /// Links `component` against the host imports and instantiates it once
    /// (so a load-time failure surfaces now, not on the first call).
    ///
    /// # Errors
    ///
    /// `plugins.load` naming the plugin: an import the host does not offer,
    /// or a failing instantiation; `plugins.call` when instantiation traps
    /// or overruns the call timeout.
    pub fn new(
        runtime: &Runtime,
        component: &Component,
        ctx: Arc<Context>,
        limits: Limits,
    ) -> Result<Self, Error> {
        let id = ctx.plugin.clone();
        let mut linker = Linker::<HostState>::new(runtime.engine());
        PluginHost::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s)
            .map_err(|e| load_error(&id, &e))?;
        let pre = linker
            .instantiate_pre(component)
            .map_err(|e| load_error(&id, &e))?;
        let exported = runtime.exported_worlds(component);
        let instance = Self {
            tool: tool::plugin::GuestIndices::new(&pre).ok(),
            hook: hook::plugin::GuestIndices::new(&pre).ok(),
            extensions: hook::extensions::GuestIndices::new(&pre).ok(),
            provider: provider::plugin::GuestIndices::new(&pre).ok(),
            surfaces: hook::surfaces::GuestIndices::new(&pre).ok(),
            models: provider::models::GuestIndices::new(&pre).ok(),
            backend: terminal_backend::plugin::GuestIndices::new(&pre).ok(),
            router: delegation_router::plugin::GuestIndices::new(&pre).ok(),
            memory: memory_provider::plugin::GuestIndices::new(&pre).ok(),
            memory_hooks: memory_provider::hooks::GuestIndices::new(&pre).ok(),
            engine: context_engine::plugin::GuestIndices::new(&pre).ok(),
            adapter: platform_adapter::plugin::GuestIndices::new(&pre).ok(),
            tool_specs: Mutex::new(None),
            id,
            pre,
            ctx,
            limits,
            exported,
            live: Mutex::new(None),
            pool: Mutex::new(Vec::new()),
            traps: Mutex::new(Traps::default()),
        };
        instance.with_live(&CallInfo::default(), None, |_, _| Ok(()))?;
        Ok(instance)
    }

    /// The plugin id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The instance context (profile, grants, ports).
    #[must_use]
    pub fn context(&self) -> &Arc<Context> {
        &self.ctx
    }

    /// The plugin worlds the component exports, with their versions.
    #[must_use]
    pub fn exported(&self) -> &[WorldRef] {
        &self.exported
    }

    /// Whether three traps within a minute suspended the instance.
    #[must_use]
    pub fn suspended(&self) -> bool {
        self.traps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .suspended
    }

    /// `tool.tools()`.
    ///
    /// # Errors
    ///
    /// `plugins.load` when the component does not export the tool world;
    /// `plugins.call` when the call traps or times out.
    pub fn tools(&self, call: &CallInfo) -> Result<Vec<tool::plugin::ToolSpec>, Error> {
        let indices = self.tool.as_ref().ok_or_else(|| self.missing("tool"))?;
        if let Some(specs) = self.cached_tools() {
            return Ok(specs);
        }
        let specs = self.with_live(call, None, |store, instance| {
            indices.load(&mut *store, instance)?.call_tools(&mut *store)
        })?;
        *self
            .tool_specs
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(specs.clone());
        Ok(specs)
    }

    fn cached_tools(&self) -> Option<Vec<tool::plugin::ToolSpec>> {
        self.tool_specs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// `tool.call(name, arguments)`: the plugin's own `Err` is returned as
    /// `Ok(Err(text))`, a trap or timeout as `Err`.
    ///
    /// # Errors
    ///
    /// As [`Self::tools`].
    pub fn call_tool(
        &self,
        call: &CallInfo,
        name: &str,
        arguments: &str,
    ) -> Result<Result<String, String>, Error> {
        let indices = self.tool.as_ref().ok_or_else(|| self.missing("tool"))?;
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_call(&mut *store, name, arguments)
        })
    }

    /// `hook.hooks()`.
    ///
    /// # Errors
    ///
    /// As [`Self::tools`], for the hook world.
    pub fn hooks(&self, call: &CallInfo) -> Result<Vec<String>, Error> {
        let indices = self.hook.as_ref().ok_or_else(|| self.missing("hook"))?;
        self.with_live(call, None, |store, instance| {
            indices.load(&mut *store, instance)?.call_hooks(&mut *store)
        })
    }

    /// `hook.on-hook(event)`.
    ///
    /// # Errors
    ///
    /// As [`Self::tools`], for the hook world.
    /// `bound` (`plugins.hook_timeout_ms`) caps the call below the
    /// instance's call timeout.
    pub fn on_hook(
        &self,
        call: &CallInfo,
        event: &hook::plugin::HookEvent,
        bound: Duration,
    ) -> Result<Result<hook::plugin::Reply, String>, Error> {
        let indices = self.hook.as_ref().ok_or_else(|| self.missing("hook"))?;
        self.with_live(call, Some(bound), |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_on_hook(&mut *store, event)
        })
    }

    /// Whether the component exports `world`'s plugin interface.
    #[must_use]
    pub fn implements(&self, world: &str) -> bool {
        match world {
            "tool" => self.tool.is_some(),
            "hook" => self.hook.is_some(),
            "provider" => self.provider.is_some(),
            "terminal-backend" => {
                self.backend.is_some() || self.exported.iter().any(|w| w.name == "terminal-backend")
            }
            other => self.exported.iter().any(|w| w.name == other),
        }
    }

    /// Whether the component exports the hook world's `extensions` (1.1).
    #[must_use]
    pub fn extends(&self) -> bool {
        self.extensions.is_some()
    }

    /// `extensions.sections()`; empty for a component without
    /// `extensions`.
    ///
    /// # Errors
    ///
    /// `plugins.call` when the call traps or times out.
    pub fn sections(&self, call: &CallInfo) -> Result<Vec<String>, Error> {
        let Some(indices) = &self.extensions else {
            return Ok(Vec::new());
        };
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_sections(&mut *store)
        })
    }

    /// `extensions.render-section(id)`.
    ///
    /// # Errors
    ///
    /// As [`Self::tools`], for `extensions`.
    pub fn render_section(
        &self,
        call: &CallInfo,
        id: &str,
    ) -> Result<Result<String, String>, Error> {
        let indices = self
            .extensions
            .as_ref()
            .ok_or_else(|| self.missing("hook@1.1"))?;
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_render_section(&mut *store, id)
        })
    }

    /// `extensions.commands()`; empty for a component without `extensions`.
    ///
    /// # Errors
    ///
    /// `plugins.call` when the call traps or times out.
    pub fn commands(&self, call: &CallInfo) -> Result<Vec<hook::extensions::Command>, Error> {
        let Some(indices) = &self.extensions else {
            return Ok(Vec::new());
        };
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_commands(&mut *store)
        })
    }

    /// `extensions.run-command(name, args)`.
    ///
    /// # Errors
    ///
    /// As [`Self::tools`], for `extensions`.
    pub fn run_command(
        &self,
        call: &CallInfo,
        name: &str,
        args: &str,
    ) -> Result<Result<String, String>, Error> {
        let indices = self
            .extensions
            .as_ref()
            .ok_or_else(|| self.missing("hook@1.1"))?;
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_run_command(&mut *store, name, args)
        })
    }

    /// `extensions.on-event(name, payload)`.
    ///
    /// # Errors
    ///
    /// As [`Self::tools`], for `extensions`.
    pub fn on_event(
        &self,
        call: &CallInfo,
        name: &str,
        payload: &str,
    ) -> Result<Result<(), String>, Error> {
        let indices = self
            .extensions
            .as_ref()
            .ok_or_else(|| self.missing("hook@1.1"))?;
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_on_event(&mut *store, name, payload)
        })
    }

    /// `provider.providers()`.
    ///
    /// # Errors
    ///
    /// As [`Self::tools`], for the provider world.
    pub fn providers(&self, call: &CallInfo) -> Result<Vec<provider::plugin::ProviderSpec>, Error> {
        let indices = self
            .provider
            .as_ref()
            .ok_or_else(|| self.missing("provider"))?;
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_providers(&mut *store)
        })
    }

    /// `provider.invoke(provider, request)`.
    ///
    /// # Errors
    ///
    /// As [`Self::tools`], for the provider world.
    pub fn invoke_provider(
        &self,
        call: &CallInfo,
        id: &str,
        request: &str,
    ) -> Result<Result<String, String>, Error> {
        let indices = self
            .provider
            .as_ref()
            .ok_or_else(|| self.missing("provider"))?;
        self.with_live(call, None, |store, instance| {
            indices
                .load(&mut *store, instance)?
                .call_invoke(&mut *store, id, request)
        })
    }

    fn missing(&self, world: &str) -> Error {
        let package = world.split('@').next().unwrap_or(world);
        Error::new(
            &codes::PLUGINS_LOAD,
            format!(
                "plugin '{}' does not export the {world} world (prometheus:{package}/plugin)",
                self.id
            ),
        )
    }

    fn check_suspended(&self) -> Result<(), Error> {
        if self.suspended() {
            return Err(Error::new(
                &codes::PLUGINS_CALL,
                format!(
                    "plugin '{}' is suspended after {TRAPS_TO_SUSPEND} traps within {}s",
                    self.id,
                    TRAP_WINDOW.as_secs()
                ),
            ));
        }
        Ok(())
    }

    /// Runs `f` on the shared live instance under the call bounds (`bound`
    /// caps the call timeout), instantiating first when needed.
    fn with_live<R>(
        &self,
        call: &CallInfo,
        bound: Option<Duration>,
        f: impl FnOnce(&mut Store<HostState>, &wasmtime::component::Instance) -> wasmtime::Result<R>,
    ) -> Result<R, Error> {
        self.check_suspended()?;
        let limits = self.limits.bounded(bound);
        let mut guard = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        match self.run(&mut guard, call, limits, Meter::wall(), f) {
            Ok(value) => Ok(value),
            Err(err) => {
                // A trapped or interrupted component cannot be re-entered.
                *guard = None;
                drop(guard);
                Err(self.trapped(&err))
            }
        }
    }

    /// Runs `f` on a store of its own (see the module docs) with the call
    /// timeout as a CPU budget and `wall` as the limit on the whole call.
    fn with_pooled<R>(
        &self,
        call: &CallInfo,
        wall: Option<Duration>,
        f: impl FnOnce(&mut Store<HostState>, &wasmtime::component::Instance) -> wasmtime::Result<R>,
    ) -> Result<R, Error> {
        self.check_suspended()?;
        let mut slot = self
            .pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        let meter = Meter::cpu(self.limits.call_timeout, wall);
        match self.run(&mut slot, call, self.limits, meter, f) {
            Ok(value) => {
                let mut pool = self.pool.lock().unwrap_or_else(PoisonError::into_inner);
                if pool.len() < POOL_IDLE {
                    pool.extend(slot);
                }
                Ok(value)
            }
            // The trapped store in `slot` is dropped, never reused.
            Err(err) => Err(self.trapped(&err)),
        }
    }

    fn trapped(&self, err: &wasmtime::Error) -> Error {
        self.record_trap();
        self.call_error(err)
    }

    fn run<R>(
        &self,
        live: &mut Option<Live>,
        call: &CallInfo,
        limits: Limits,
        meter: Meter,
        f: impl FnOnce(&mut Store<HostState>, &wasmtime::component::Instance) -> wasmtime::Result<R>,
    ) -> wasmtime::Result<R> {
        if live.is_none() {
            let mut store = self.store(call)?;
            let instance = self.pre.instantiate(&mut store)?;
            *live = Some(Live { store, instance });
        }
        let Some(live) = live.as_mut() else {
            return Err(wasmtime::format_err!("instance missing"));
        };
        live.store.data_mut().call = call.clone();
        Self::arm(&mut live.store, limits, meter)?;
        f(&mut live.store, &live.instance)
    }

    fn store(&self, call: &CallInfo) -> wasmtime::Result<Store<HostState>> {
        let memory = usize::try_from(self.limits.max_memory_bytes).unwrap_or(usize::MAX);
        let limits = StoreLimitsBuilder::new()
            .memory_size(memory)
            .trap_on_grow_failure(true)
            .build();
        let mut store = Store::new(
            self.pre.engine(),
            HostState {
                ctx: Arc::clone(&self.ctx),
                call: call.clone(),
                limits,
                console: None,
                meter: Meter::wall(),
            },
        );
        store.limiter(|state| &mut state.limits);
        store.call_hook(|mut store, hook| {
            store.data_mut().meter.transition(hook);
            Ok(())
        });
        store.epoch_deadline_callback(|store| Ok(store.data().meter.on_deadline()));
        Self::arm(&mut store, self.limits, Meter::wall())?;
        Ok(store)
    }

    /// Fresh fuel, epoch deadline and meter for one call.
    fn arm(store: &mut Store<HostState>, limits: Limits, meter: Meter) -> wasmtime::Result<()> {
        store.set_fuel(limits.fuel())?;
        store.set_epoch_deadline(limits.epoch_ticks());
        store.data_mut().meter = meter;
        Ok(())
    }

    fn record_trap(&self) {
        let mut traps = self.traps.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        traps.recent.push_back(now);
        while traps
            .recent
            .front()
            .is_some_and(|t| now.duration_since(*t) > TRAP_WINDOW)
        {
            traps.recent.pop_front();
        }
        if traps.recent.len() >= TRAPS_TO_SUSPEND && !traps.suspended {
            traps.suspended = true;
            tracing::warn!(
                plugin = %self.id,
                profile = %self.ctx.profile,
                "plugin suspended after {TRAPS_TO_SUSPEND} traps within {}s",
                TRAP_WINDOW.as_secs()
            );
        }
    }

    fn call_error(&self, err: &wasmtime::Error) -> Error {
        let timeout = matches!(
            err.downcast_ref::<Trap>(),
            Some(Trap::Interrupt | Trap::OutOfFuel)
        );
        let message = if timeout {
            format!("plugin '{}' exceeded call timeout", self.id)
        } else {
            format!("plugin '{}' trapped: {}", self.id, root_cause(err))
        };
        tracing::warn!(plugin = %self.id, profile = %self.ctx.profile, "{message}");
        Error::new(&codes::PLUGINS_CALL, message)
    }
}

fn root_cause(err: &wasmtime::Error) -> String {
    err.chain()
        .last()
        .map_or_else(|| err.to_string(), ToString::to_string)
}

fn load_error(id: &str, err: &wasmtime::Error) -> Error {
    Error::new(
        &codes::PLUGINS_LOAD,
        format!("plugin '{id}' cannot be linked: {err:#}"),
    )
    .with_hint("plugins may import only the prometheus:host interfaces (no WASI)")
}
