//! Plugin hook callbacks (spec: plugin-system, Typed hook registry; Hook
//! isolation and time bounds; `pre_tool_call` directives; `pre_llm_call`
//! context preserves the prompt cache).
//!
//! The registry is per profile and holds, for each hook of the closed list
//! (`pmt_core::hooks`), the callbacks of the loaded `hook`-world plugins in
//! plugin load order, then registration order. A plugin that subscribes to
//! a name outside the list fails to load with `unknown hook '<name>'`; a
//! known hook its grant does not cover is skipped with a warning. Firing a
//! hook runs every callback in that order, each bounded by
//! `plugins.hook_timeout_ms`: an error is logged (WARN once per plugin,
//! hook and error, DEBUG after) and the next callback still runs; a
//! callback that overruns counts as no result and is skipped for 60
//! seconds.

mod context;
mod directives;
pub mod payload;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use pmt_core::capability::{Capability, CapabilitySet, PluginRight};
use pmt_core::error::{Error, codes};

pub use context::{SPILL_EDGE_CHARS, spill_dir};
pub use directives::{PluginToolHooks, PreTool, combine};
pub use payload::{Payload, SCHEMA_VERSION};

use crate::host::hook_world::{HookEvent, Reply};
use crate::host::{CallInfo, Instance};

/// How long a timed-out callback is skipped.
pub const TIMEOUT_SUSPENSION: Duration = Duration::from_mins(1);

/// Scheduling slack on top of the hook bound before the host stops
/// waiting (the instance's own deadline normally fires first).
const WAIT_SLACK: Duration = Duration::from_secs(2);

/// What every hook firing needs.
#[derive(Debug, Clone)]
pub struct HookSettings {
    /// The profile's home directory (`hook_outputs/`).
    pub profile_dir: PathBuf,
    /// `plugins.hook_timeout_ms`.
    pub timeout: Duration,
    /// `plugins.hook_context_max_chars`.
    pub context_max_chars: usize,
}

/// One callback's answer.
#[derive(Debug, Clone)]
pub struct Answer {
    /// The plugin that answered.
    pub plugin: String,
    /// What it answered.
    pub reply: Reply,
}

struct Callback {
    plugin: String,
    instance: Arc<Instance>,
    /// The hook world minor version the plugin was built against.
    minor: u64,
}

/// A profile's hook callbacks.
pub struct Hooks {
    settings: HookSettings,
    callbacks: HashMap<String, Vec<Callback>>,
    suspended: Mutex<HashMap<(String, String), Instant>>,
    warned: Mutex<HashSet<(String, String, String)>>,
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut hooks: Vec<&String> = self.callbacks.keys().collect();
        hooks.sort();
        f.debug_struct("Hooks")
            .field("hooks", &hooks)
            .finish_non_exhaustive()
    }
}

impl Hooks {
    /// An empty registry.
    #[must_use]
    pub fn new(settings: HookSettings) -> Self {
        Self {
            settings,
            callbacks: HashMap::new(),
            suspended: Mutex::new(HashMap::new()),
            warned: Mutex::new(HashSet::new()),
        }
    }

    /// The settings.
    #[must_use]
    pub fn settings(&self) -> &HookSettings {
        &self.settings
    }

    /// Registers the hooks `instance` subscribes to (its `hooks()`), in its
    /// order; returns the registered names.
    ///
    /// # Errors
    ///
    /// `plugins.manifest`: `unknown hook '<name>'` for a name outside the
    /// closed list (nothing of the plugin is registered then);
    /// `plugins.call` when `hooks()` fails.
    pub fn register(
        &mut self,
        instance: &Arc<Instance>,
        granted: &CapabilitySet,
        call: &CallInfo,
    ) -> Result<Vec<String>, Error> {
        let names = instance.hooks(call)?;
        if let Some(unknown) = names.iter().find(|n| pmt_core::hooks::check(n).is_err()) {
            return Err(Error::new(
                &codes::PLUGINS_MANIFEST,
                format!("unknown hook '{unknown}'"),
            ));
        }
        let minor = instance
            .exported()
            .iter()
            .find(|w| w.name == "hook")
            .map_or(0, |w| w.version.minor);
        let mut registered = Vec::new();
        for name in names {
            let right = Capability::Plugin {
                right: PluginRight::Hook,
                target: Some(name.clone()),
            };
            if !granted.allows(&right) {
                tracing::warn!(
                    plugin = instance.id(),
                    "capability_denied: hooks {name}; the callback is not registered"
                );
                continue;
            }
            self.callbacks
                .entry(name.clone())
                .or_default()
                .push(Callback {
                    plugin: instance.id().to_owned(),
                    instance: Arc::clone(instance),
                    minor,
                });
            registered.push(name);
        }
        Ok(registered)
    }

    /// Whether any callback handles `hook`.
    #[must_use]
    pub fn handles(&self, hook: &str) -> bool {
        self.callbacks.get(hook).is_some_and(|c| !c.is_empty())
    }

    /// Runs every callback of `hook` in order and returns the answers of
    /// those that answered in time without error.
    pub async fn fire(&self, hook: &str, payload: &Payload, call: &CallInfo) -> Vec<Answer> {
        let Some(callbacks) = self.callbacks.get(hook) else {
            return Vec::new();
        };
        let mut answers = Vec::new();
        for callback in callbacks {
            if self.is_suspended(&callback.plugin, hook) {
                continue;
            }
            let event = HookEvent {
                hook: hook.to_owned(),
                schema_version: SCHEMA_VERSION.to_owned(),
                payload: payload.for_minor(hook, callback.minor),
            };
            if let Some(reply) = self.run(callback, hook, event, call).await {
                answers.push(Answer {
                    plugin: callback.plugin.clone(),
                    reply,
                });
            }
        }
        answers
    }

    async fn run(
        &self,
        callback: &Callback,
        hook: &str,
        event: HookEvent,
        call: &CallInfo,
    ) -> Option<Reply> {
        let instance = Arc::clone(&callback.instance);
        let bound = self.settings.timeout;
        let call = call.clone();
        let task = tokio::task::spawn_blocking(move || instance.on_hook(&call, &event, bound));
        let outcome = match tokio::time::timeout(bound + WAIT_SLACK, task).await {
            Err(_) => Err(timed_out(&callback.plugin)),
            Ok(Err(join)) => Err(Error::new(&codes::PLUGINS_CALL, join.to_string())),
            Ok(Ok(result)) => result,
        };
        match outcome {
            Ok(Ok(reply)) => Some(reply),
            Ok(Err(message)) => {
                self.report(&callback.plugin, hook, &message);
                None
            }
            Err(err) => {
                if err.message().ends_with("exceeded call timeout") {
                    self.suspend(&callback.plugin, hook);
                }
                self.report(&callback.plugin, hook, err.message());
                None
            }
        }
    }

    fn is_suspended(&self, plugin: &str, hook: &str) -> bool {
        let mut suspended = self
            .suspended
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let key = (plugin.to_owned(), hook.to_owned());
        match suspended.get(&key) {
            Some(until) if Instant::now() < *until => true,
            Some(_) => {
                suspended.remove(&key);
                false
            }
            None => false,
        }
    }

    fn suspend(&self, plugin: &str, hook: &str) {
        self.suspended
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                (plugin.to_owned(), hook.to_owned()),
                Instant::now() + TIMEOUT_SUSPENSION,
            );
    }

    /// WARN the first time a plugin, hook and error are seen, DEBUG after.
    fn report(&self, plugin: &str, hook: &str, error: &str) {
        let first = self
            .warned
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((plugin.to_owned(), hook.to_owned(), error.to_owned()));
        if first {
            tracing::warn!(plugin, hook, "hook callback failed: {error}");
        } else {
            tracing::debug!(plugin, hook, "hook callback failed again: {error}");
        }
    }
}

fn timed_out(plugin: &str) -> Error {
    Error::new(
        &codes::PLUGINS_CALL,
        format!("plugin '{plugin}' exceeded call timeout"),
    )
}
