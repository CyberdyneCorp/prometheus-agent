//! The wasmtime component runtime (spec: plugin-system, WASM sandbox
//! resource limits; Host interfaces offered to plugins; Plugin state and
//! profile scope).
//!
//! One [`Runtime`] (engine) serves the process. Each call into a plugin
//! runs under a fuel budget and an epoch deadline derived from
//! `plugins.wasm.call_timeout_ms` (a background thread advances the epoch
//! every [`TICK`]), with linear memory capped at
//! `plugins.wasm.max_memory_bytes`. Components get only the WIT host
//! imports: no WASI, so no ambient filesystem, network, clock or
//! environment.

#[allow(missing_docs, clippy::all, clippy::pedantic)]
mod bindings;
pub mod imports;
mod instance;
pub mod ports;
pub mod state;
mod subagents;

use std::time::Duration;

use pmt_config::schema::PluginsWasmConfig;
use pmt_core::error::{Error, codes};
use wasmtime::component::Component;

pub use bindings::exports::prometheus::delegation_router::plugin as delegation_router_world;
pub use bindings::exports::prometheus::hook::extensions as extensions_world;
pub use bindings::exports::prometheus::hook::plugin as hook_world;
pub use bindings::exports::prometheus::hook::surfaces as surfaces_world;
pub use bindings::exports::prometheus::provider::plugin as provider_world;
pub use bindings::exports::prometheus::terminal_backend::plugin as terminal_backend_world;
pub use bindings::exports::prometheus::tool::plugin as tool_world;
pub use imports::{CallInfo, Console, Context};
pub use instance::Instance;

/// How often the epoch advances; a call overruns its deadline by at most
/// about one tick plus scheduling.
pub const TICK: Duration = Duration::from_millis(10);

/// Fuel granted per millisecond of call timeout. Fuel is the deterministic
/// backstop; the epoch deadline is what normally stops a runaway call, so
/// this is set above what a fast machine executes per millisecond.
const FUEL_PER_MS: u64 = 5_000_000;

/// The resource bounds of one instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Linear memory cap.
    pub max_memory_bytes: u64,
    /// CPU bound of one call.
    pub call_timeout: Duration,
}

impl Limits {
    /// From `[plugins.wasm]`.
    #[must_use]
    pub fn from_config(config: &PluginsWasmConfig) -> Self {
        Self {
            max_memory_bytes: config.max_memory_bytes,
            call_timeout: Duration::from_millis(config.call_timeout_ms),
        }
    }

    /// These limits with the call timeout capped at `bound`.
    fn bounded(self, bound: Option<Duration>) -> Self {
        Self {
            call_timeout: bound.map_or(self.call_timeout, |b| b.min(self.call_timeout)),
            ..self
        }
    }

    fn fuel(&self) -> u64 {
        let ms = u64::try_from(self.call_timeout.as_millis()).unwrap_or(u64::MAX);
        ms.saturating_mul(FUEL_PER_MS).max(FUEL_PER_MS)
    }

    fn epoch_ticks(&self) -> u64 {
        ticks(self.call_timeout)
    }
}

/// Epoch ticks covering `span`, plus one for the tick in progress.
fn ticks(span: Duration) -> u64 {
    let tick = TICK.as_millis().max(1);
    u64::try_from(span.as_millis().div_ceil(tick)).unwrap_or(u64::MAX) + 1
}

/// The process's wasmtime engine and its epoch ticker.
#[derive(Clone)]
pub struct Runtime {
    engine: wasmtime::Engine,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime").finish_non_exhaustive()
    }
}

impl Runtime {
    /// An engine with fuel, epoch interruption and the component model,
    /// and a thread advancing its epoch until the engine is dropped.
    ///
    /// # Errors
    ///
    /// `plugins.load` when the engine cannot be created on this host.
    pub fn new() -> Result<Self, Error> {
        let mut config = wasmtime::Config::new();
        config
            .wasm_component_model(true)
            .consume_fuel(true)
            .epoch_interruption(true);
        let engine = wasmtime::Engine::new(&config).map_err(|e| {
            Error::new(
                &codes::PLUGINS_LOAD,
                format!("cannot start the WASM engine: {e}"),
            )
        })?;
        let weak = engine.weak();
        std::thread::Builder::new()
            .name("pmt-plugin-epoch".to_owned())
            .spawn(move || {
                while let Some(engine) = weak.upgrade() {
                    engine.increment_epoch();
                    drop(engine);
                    std::thread::sleep(TICK);
                }
            })
            .map_err(|e| {
                Error::new(
                    &codes::PLUGINS_LOAD,
                    format!("cannot start the plugin epoch thread: {e}"),
                )
            })?;
        Ok(Self { engine })
    }

    /// Compiles a component.
    ///
    /// # Errors
    ///
    /// `plugins.load` naming the plugin when the bytes are not a valid
    /// component.
    pub fn compile(&self, plugin: &str, bytes: &[u8]) -> Result<Component, Error> {
        Component::new(&self.engine, bytes).map_err(|e| {
            Error::new(
                &codes::PLUGINS_LOAD,
                format!("plugin '{plugin}' is not a valid component: {e:#}"),
            )
        })
    }

    /// The plugin worlds a component exports, with the versions it was
    /// built against.
    #[must_use]
    pub fn exported_worlds(&self, component: &Component) -> Vec<crate::worlds::WorldRef> {
        component
            .component_type()
            .exports(&self.engine)
            .filter_map(|(name, _)| crate::worlds::WorldRef::from_export(name))
            .collect()
    }

    /// The names a component imports.
    #[must_use]
    pub fn imports(&self, component: &Component) -> Vec<String> {
        component
            .component_type()
            .imports(&self.engine)
            .map(|(name, _)| name.to_owned())
            .collect()
    }

    pub(crate) fn engine(&self) -> &wasmtime::Engine {
        &self.engine
    }
}
