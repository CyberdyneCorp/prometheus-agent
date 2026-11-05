//! Typed gateway hooks (spec: messaging-gateway, Gateway lifecycle hooks
//! are typed; Gateway slash commands, quick commands and hooks). Hooks are
//! registered per profile under names of the closed registry
//! ([`pmt_core::hooks`]); an event runs only the hooks of its own
//! profile, each bounded by a timeout, and a failing or slow hook is
//! logged and never blocks the pipeline. `pre_command` answers with a
//! typed [`PreCommand`].

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use pmt_core::error::Error;
use serde::Deserialize;
use serde_json::{Value, json};

/// One hook callback: takes the event payload, answers a JSON value
/// (only `pre_command` answers are read).
pub trait Hook: Send + Sync {
    /// Runs the hook.
    fn call(
        &self,
        name: &str,
        payload: &Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, Error>> + Send + '_>>;
}

/// What a `pre_command` hook decides.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PreCommand {
    /// Run the command.
    Continue,
    /// Do not run it; reply `message`.
    Deny {
        /// The reply.
        message: String,
    },
    /// The hook handled it; nothing else runs.
    Handled,
    /// Process `text` instead of the command line.
    Rewrite {
        /// The replacement input.
        text: String,
    },
}

/// The default bound of one callback (`plugins.hook_timeout_ms`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// One profile's hooks: event name and callback, in registration order.
type ProfileHooks = Vec<(String, Arc<dyn Hook>)>;

/// Where a profile's other hook callbacks live (the embedder's plugin
/// hooks, shell hooks and hook directories): one callback per profile
/// that runs them for every event, after the hooks registered here.
pub trait HookBridge: Send + Sync {
    /// The bridge callback of `profile`, if it has one.
    fn hook(&self, profile: &str) -> Option<Arc<dyn Hook>>;
}

/// Hooks by profile and name.
pub struct Hooks {
    by_profile: RwLock<HashMap<String, ProfileHooks>>,
    timeout: Duration,
    bridge: RwLock<Option<Arc<dyn HookBridge>>>,
}

impl Default for Hooks {
    fn default() -> Self {
        Self::new(DEFAULT_TIMEOUT)
    }
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hooks").finish_non_exhaustive()
    }
}

impl Hooks {
    /// No hooks; callbacks bounded by `timeout`.
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self {
            by_profile: RwLock::default(),
            timeout,
            bridge: RwLock::default(),
        }
    }

    /// Runs `bridge`'s callback of the event's profile after the hooks
    /// registered here, for every event.
    ///
    /// # Panics
    ///
    /// When the hooks lock is poisoned.
    pub fn set_bridge(&self, bridge: Arc<dyn HookBridge>) {
        *self.bridge.write().expect("hooks poisoned") = Some(bridge);
    }

    /// Registers `hook` for event `name` of `profile`.
    ///
    /// # Errors
    ///
    /// `registry.unknown_name` (`unknown hook '<name>'`) for a name outside
    /// the closed registry.
    ///
    /// # Panics
    ///
    /// When the hooks lock is poisoned.
    pub fn register(&self, profile: &str, name: &str, hook: Arc<dyn Hook>) -> Result<(), Error> {
        pmt_core::hooks::check(name)?;
        self.by_profile
            .write()
            .expect("hooks poisoned")
            .entry(profile.to_owned())
            .or_default()
            .push((name.to_owned(), hook));
        Ok(())
    }

    fn of(&self, profile: &str, name: &str) -> Vec<Arc<dyn Hook>> {
        let mut hooks: Vec<Arc<dyn Hook>> = self
            .by_profile
            .read()
            .expect("hooks poisoned")
            .get(profile)
            .map(|hooks| {
                hooks
                    .iter()
                    .filter(|(n, _)| n == name)
                    .map(|(_, h)| Arc::clone(h))
                    .collect()
            })
            .unwrap_or_default();
        let bridge = self.bridge.read().expect("hooks poisoned").clone();
        hooks.extend(bridge.and_then(|b| b.hook(profile)));
        hooks
    }

    async fn run(&self, hook: &dyn Hook, name: &str, payload: &Value) -> Option<Value> {
        let timeout = self.timeout;
        match tokio::time::timeout(timeout, hook.call(name, payload)).await {
            Ok(Ok(value)) => Some(value),
            Ok(Err(err)) => {
                tracing::warn!(hook = name, "hook failed: {err}");
                None
            }
            Err(_) => {
                tracing::warn!(hook = name, "hook timed out after {timeout:?}");
                None
            }
        }
    }

    /// Emits an observer event to `profile`'s hooks; results are ignored.
    pub async fn emit(&self, profile: &str, name: &str, payload: Value) {
        let payload = with_schema(payload);
        for hook in self.of(profile, name) {
            let _ = self.run(hook.as_ref(), name, &payload).await;
        }
    }

    /// Asks `profile`'s `pre_command` hooks about `command`: the first
    /// answer other than `continue` wins; unreadable answers count as
    /// `continue`.
    pub async fn pre_command(&self, profile: &str, command: &str, args: &str) -> PreCommand {
        let payload = with_schema(json!({"command": command, "args": args}));
        for hook in self.of(profile, "pre_command") {
            let Some(answer) = self.run(hook.as_ref(), "pre_command", &payload).await else {
                continue;
            };
            match serde_json::from_value::<PreCommand>(answer) {
                Ok(PreCommand::Continue) => {}
                Ok(decision) => return decision,
                Err(err) => tracing::warn!("pre_command hook answer ignored: {err}"),
            }
        }
        PreCommand::Continue
    }
}

fn with_schema(mut payload: Value) -> Value {
    if let Some(fields) = payload.as_object_mut() {
        fields.insert("schema_version".into(), "prometheus.hooks.v1".into());
    }
    payload
}
