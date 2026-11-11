//! The binary's side of the plugin host (spec: plugin-system): one wasmtime
//! runtime per process, started only when some plugin is to be loaded, and
//! one live set of plugin instances per profile, shared by every session
//! of the profile in the process.
//!
//! The set is replaced as a whole: when its plugins, their files, their
//! grants or `[plugins]` change (checked when a session starts) or on an
//! explicit reload (spec: Hot reload respects the prompt-cache invariant).
//! Calls in flight finish on the instances they started on, which are
//! dropped afterwards. Hooks, slash commands and event subscribers are
//! read from the current set at each use, so a reload activates them at
//! once in running sessions; tools, system-prompt sections and skills are
//! read when a session starts and stay as they were for its lifetime.

pub mod approvals;
pub mod bridge;
pub mod catalog;
#[cfg(feature = "gateway")]
pub mod gateway;
pub mod llm;
#[cfg(feature = "gateway")]
pub mod messaging;
mod outside;
mod ports;
pub mod providers;
pub mod reload;
pub mod subagents;
pub mod terminal;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use pmt_agent::turn::model::BoxFuture;
use pmt_agent::turn::{FinalReply, Transformed, TurnHooks, Verify};
use pmt_config::ProfileScope;
use pmt_plugins::hooks::{HookSettings, HookSource, Hooks, PluginToolHooks};
use pmt_plugins::hooks::{Observers, Payload, Rewrite};
use pmt_plugins::host::{CallInfo, Runtime};
use pmt_plugins::load::{LoadInputs, Loaded, State, Status, load, survey};
use pmt_tools::dispatch::ToolHooks;
use semver::Version;
use serde_json::{Map, Value, json};

/// The running Prometheus version, as plugin manifests compare it.
#[must_use]
pub fn running_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::new(0, 0, 0))
}

/// One load of a profile's plugins.
struct Generation {
    number: u64,
    fingerprint: String,
    loaded: Arc<Loaded>,
}

/// A profile's live plugin set.
struct Handle {
    current: Mutex<Arc<Generation>>,
}

impl Handle {
    fn generation(&self) -> Arc<Generation> {
        Arc::clone(&self.current.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl HookSource for Handle {
    fn hooks(&self) -> Arc<Hooks> {
        Arc::clone(&self.generation().loaded.hooks)
    }
}

/// What one profile's sessions share: a handle on its live plugin set.
#[derive(Clone)]
pub struct ProfilePlugins {
    handle: Arc<Handle>,
}

impl ProfilePlugins {
    /// The current set.
    #[must_use]
    pub fn current(&self) -> Arc<Loaded> {
        Arc::clone(&self.handle.generation().loaded)
    }

    /// The number of the current set; it changes on every replacement.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.handle.generation().number
    }

    /// The dispatcher's hook seam for one session: the hooks of whatever
    /// set is current at each call.
    #[must_use]
    pub fn tool_hooks(&self, call: CallInfo) -> Arc<dyn ToolHooks> {
        Arc::new(PluginToolHooks::live(
            Arc::clone(&self.handle) as Arc<dyn HookSource>,
            call,
        ))
    }

    /// [`ProfilePlugins::tool_hooks`] behind the first-party
    /// `security-guidance` plugin when `config` enables it.
    #[must_use]
    pub fn tool_hooks_with_guidance(
        &self,
        call: CallInfo,
        config: &pmt_config::schema::PluginsConfig,
    ) -> Arc<dyn ToolHooks> {
        let hooks = self.tool_hooks(call);
        let id = pmt_tools::guidance::PLUGIN_ID;
        let enabled =
            config.enabled.iter().any(|p| p == id) && !config.disabled.iter().any(|p| p == id);
        if !enabled {
            return hooks;
        }
        pmt_tools::guidance::GuidanceHooks::wrap(config.security_guidance.mode, hooks)
    }

    /// The provider caller's `transform_api_error_classification` seam.
    #[must_use]
    pub fn error_classifier(
        &self,
        call: CallInfo,
    ) -> Arc<dyn pmt_providers::caller::ErrorClassifier> {
        Arc::new(pmt_plugins::hooks::PluginErrorClassifier::new(
            Arc::clone(&self.handle) as Arc<dyn HookSource>,
            call,
        ))
    }

    /// The turn loop's hook seam for one session.
    #[must_use]
    pub fn turn_hooks(&self, call: CallInfo) -> Arc<dyn TurnHooks> {
        let observers = Observers::start(
            Arc::clone(&self.handle) as Arc<dyn HookSource>,
            call.clone(),
        );
        Arc::new(SessionTurnHooks {
            handle: Arc::clone(&self.handle),
            call,
            observers,
        })
    }

    /// Fires an observer hook of this profile and waits for its callbacks
    /// (whatever they answer is discarded).
    pub async fn fire_observer(&self, call: &CallInfo, hook: &str, fields: Map<String, Value>) {
        let hooks = self.handle.hooks();
        if hooks.handles(hook) {
            hooks.fire(hook, &Payload::new(fields), call).await;
        }
    }

    /// Queues an observer hook of this profile (fired off the caller's
    /// path; answers discarded).
    pub fn observe(&self, call: CallInfo, hook: &'static str, fields: Map<String, Value>) {
        let hooks = self.handle.hooks();
        if !hooks.handles(hook) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        pmt_core::spawn::spawn_on(&runtime, async move {
            hooks.fire(hook, &Payload::new(fields), &call).await;
        });
    }
}

struct SessionTurnHooks {
    handle: Arc<Handle>,
    call: CallInfo,
    observers: Observers,
}

impl TurnHooks for SessionTurnHooks {
    fn holds_stream(&self) -> bool {
        self.handle.hooks().holds_stream()
    }

    fn transform_llm_output<'a>(
        &'a self,
        reply: FinalReply<'a>,
        held: bool,
    ) -> BoxFuture<'a, Transformed> {
        let hooks = self.handle.hooks();
        Box::pin(async move {
            let fields = object(json!({
                "session_id": reply.session_id,
                "response": reply.text,
                "model": reply.model,
                "platform": reply.platform,
            }));
            match hooks.transform_llm_output(fields, &self.call, held).await {
                Rewrite::Unchanged => Transformed::Unchanged,
                Rewrite::Replaced(text) => Transformed::Replaced(text),
                Rewrite::Withheld => Transformed::Withheld,
            }
        })
    }

    fn pre_verify<'a>(&'a self, verify: Verify<'a>) -> BoxFuture<'a, Option<String>> {
        let hooks = self.handle.hooks();
        Box::pin(async move {
            let fields = object(json!({
                "session_id": verify.session_id,
                "platform": verify.platform,
                "model": verify.model,
                "coding": verify.coding,
                "attempt": verify.attempt,
                "final_response": verify.final_response,
                "changed_paths": verify.changed_paths,
            }));
            hooks.pre_verify(fields, &self.call).await
        })
    }

    fn observe(&self, hook: &'static str, payload: Map<String, Value>) {
        // Checked here so a stream nobody watches queues nothing per token.
        if self.handle.hooks().handles(hook) {
            self.observers.fire(hook, payload);
        }
    }

    fn pre_llm_call<'a>(
        &'a self,
        session_id: &'a str,
        user_message: &'a str,
    ) -> BoxFuture<'a, Option<String>> {
        let hooks = self.handle.hooks();
        Box::pin(async move {
            if !hooks.handles("pre_llm_call") {
                return None;
            }
            hooks
                .pre_llm_call(session_id, user_message, &self.call)
                .await
        })
    }
}

/// The `on_skill_lifecycle` payload.
#[must_use]
pub fn skill_fields(skill: &str, action: &str) -> Map<String, Value> {
    object(json!({ "skill": skill, "action": action }))
}

/// Fires `on_skill_lifecycle` from a command outside any session (`skills
/// install|enable|disable`) and waits for the callbacks, so they run
/// before the process exits.
pub fn skill_changed(scope: &ProfileScope, skill: &str, action: &str) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| scope.dir().to_path_buf());
    let plugins = for_session(scope, &cwd);
    let each = vec![skill_fields(skill, action)];
    outside::fire_blocking(&plugins.handle.hooks(), "on_skill_lifecycle", each, &cwd);
}

/// Fires `on_session_delete` for every session a command deleted and
/// waits for the callbacks.
pub fn sessions_deleted(scope: &ProfileScope, sessions: &[String]) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| scope.dir().to_path_buf());
    let plugins = for_session(scope, &cwd);
    let each = sessions
        .iter()
        .map(|id| object(json!({ "session_id": id })))
        .collect();
    outside::fire_blocking(&plugins.handle.hooks(), "on_session_delete", each, &cwd);
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// Every profile's live set in this process, by profile directory.
fn handles() -> &'static Mutex<HashMap<PathBuf, Arc<Handle>>> {
    static HANDLES: OnceLock<Mutex<HashMap<PathBuf, Arc<Handle>>>> = OnceLock::new();
    HANDLES.get_or_init(Mutex::default)
}

/// The profile's live set for work outside a session (gateway events):
/// re-checked against the disk at most every `every`.
#[must_use]
pub fn current(scope: &ProfileScope, every: std::time::Duration) -> ProfilePlugins {
    static CHECKED: OnceLock<Mutex<HashMap<PathBuf, std::time::Instant>>> = OnceLock::new();
    let key = scope.dir().to_path_buf();
    let mut last = CHECKED
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        .copied();
    let existing = handles()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        .cloned();
    let due = due(&mut last, every);
    if let Some(at) = last {
        CHECKED
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, at);
    }
    match existing {
        Some(handle) if !due => ProfilePlugins { handle },
        _ => refresh(scope, scope.dir(), false),
    }
}

/// Whether a check is due (`last` is updated when it is).
fn due(last: &mut Option<std::time::Instant>, every: std::time::Duration) -> bool {
    let now = std::time::Instant::now();
    match last {
        Some(at) if now.duration_since(*at) < every => false,
        _ => {
            *last = Some(now);
            true
        }
    }
}

/// The profile's plugins for a session in `cwd`: the live set, replaced
/// first when something it was loaded from changed. Plugin failures never
/// fail the session; they are logged and shown by `prometheus plugins
/// list`.
#[must_use]
pub fn for_session(scope: &ProfileScope, cwd: &Path) -> ProfilePlugins {
    refresh(scope, cwd, false)
}

/// Replaces the profile's live set with a fresh load even when nothing
/// changed (`plugins reload`).
#[must_use]
pub fn reload(scope: &ProfileScope, cwd: &Path) -> ProfilePlugins {
    refresh(scope, cwd, true)
}

fn refresh(scope: &ProfileScope, cwd: &Path, force: bool) -> ProfilePlugins {
    let version = running_version();
    let blocked = catalog::kill_list(scope);
    let inputs = inputs(scope, cwd, &version, &blocked);
    let statuses: Vec<Status> = survey(&inputs).into_iter().map(|(_, s)| s).collect();
    let fingerprint = fingerprint(scope, &statuses);
    let key = scope.dir().to_path_buf();
    let existing = handles()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        .cloned();
    if let Some(handle) = &existing
        && !force
        && handle.generation().fingerprint == fingerprint
    {
        return ProfilePlugins {
            handle: Arc::clone(handle),
        };
    }
    let loaded = Arc::new(load_now(scope, &inputs, statuses));
    let handle = existing.unwrap_or_else(|| {
        let first = Arc::new(Generation {
            number: 0,
            fingerprint: String::new(),
            loaded: Arc::clone(&loaded),
        });
        let handle = Arc::new(Handle {
            current: Mutex::new(first),
        });
        Arc::clone(
            handles()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(key)
                .or_insert(handle),
        )
    });
    {
        let mut current = handle
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *current = Arc::new(Generation {
            number: current.number + 1,
            fingerprint,
            loaded: Arc::clone(&loaded),
        });
    }
    outside::plugins_loaded(&loaded, cwd);
    ProfilePlugins { handle }
}

/// Loads when some plugin is to be instantiated; otherwise reports the
/// survey without starting the WASM engine.
fn load_now(scope: &ProfileScope, inputs: &LoadInputs<'_>, statuses: Vec<Status>) -> Loaded {
    audit_tampered(scope, &statuses);
    let settings = hook_settings(scope);
    let wasm = |s: &Status| {
        s.state == State::Loaded && s.kind != Some(pmt_plugins::manifest::Kind::Python)
    };
    let mut loaded = if statuses.iter().any(wasm) {
        match runtime() {
            Some(runtime) => load(runtime, &ports::ports(scope), inputs),
            None => Loaded::empty(engine_unavailable(statuses), settings),
        }
    } else {
        Loaded::empty(statuses, settings)
    };
    // Python plugins run in bridge sidecars; their callbacks follow the
    // WASM plugins'.
    bridge::attach(scope, inputs, &mut loaded);
    // Shell hooks, hook directories and webhooks run after the plugins'
    // callbacks; the set was just built, so nothing else holds it yet.
    if let Some(hooks) = Arc::get_mut(&mut loaded.hooks) {
        crate::hooks_host::extend(scope, hooks);
    } else {
        tracing::warn!("shell hooks not registered: the hook set is already shared");
    }
    loaded
}

/// Records each plugin refused because its files changed after install in
/// the profile's security audit log.
fn audit_tampered(scope: &ProfileScope, statuses: &[Status]) {
    use pmt_core::audit::{AuditLog, Event, Kind};
    let marker = pmt_plugins::load::MODIFIED_AFTER_INSTALL;
    for status in statuses {
        if let Some(problem) = status.problem.as_deref().filter(|p| p.ends_with(marker)) {
            AuditLog::of(scope.dir()).record(
                &Event::new(Kind::Signature, format!("plugin {}", status.id), "refused")
                    .with_detail(problem.to_owned()),
            );
        }
    }
}

/// The statuses of a load the WASM engine could not run: every plugin
/// that was to load is reported as not loaded.
fn engine_unavailable(statuses: Vec<Status>) -> Vec<Status> {
    statuses
        .into_iter()
        .map(|mut status| {
            if status.state == State::Loaded {
                status.state = State::Error;
                status.problem = Some("the WASM engine could not start".to_owned());
            }
            status
        })
        .collect()
}

fn hook_settings(scope: &ProfileScope) -> HookSettings {
    let config = &scope.config().plugins;
    HookSettings {
        profile_dir: scope.dir().to_path_buf(),
        timeout: std::time::Duration::from_millis(config.hook_timeout_ms),
        context_max_chars: usize::try_from(config.hook_context_max_chars).unwrap_or(usize::MAX),
    }
}

/// The process's runtime (`None`, logged once, when the engine cannot start).
fn runtime() -> Option<&'static Runtime> {
    static RUNTIME: OnceLock<Option<Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            Runtime::new()
                .inspect_err(|err| tracing::error!("plugins unavailable: {}", err.message()))
                .ok()
        })
        .as_ref()
}

/// What decides whether a live set is still current: the plugins'
/// identities and states, their settings and the grants file.
fn fingerprint(scope: &ProfileScope, statuses: &[Status]) -> String {
    let grants = std::fs::metadata(scope.dir().join("grants.toml"))
        .ok()
        .map(|m| (m.len(), m.modified().ok()));
    // The whole section: limits, hook bounds and the settings the config
    // port serves.
    let mut text = format!(
        "{grants:?}|{:?}|{}|",
        scope.config().plugins,
        crate::hooks_host::fingerprint(scope)
    );
    for status in statuses {
        let _ = write!(
            text,
            "{}={:?}:{:?}:{:?};",
            status.id, status.digest, status.state, status.problem
        );
    }
    text
}

/// The load inputs of `scope` for a session in `cwd`, under the kill list
/// `blocked` ([`catalog::kill_list`]).
#[must_use]
pub fn inputs<'a>(
    scope: &'a ProfileScope,
    cwd: &'a Path,
    version: &'a Version,
    blocked: &'a [pmt_plugins::catalog::Removed],
) -> LoadInputs<'a> {
    LoadInputs {
        profile: scope.name().as_str(),
        profile_dir: scope.dir(),
        cwd: Some(cwd),
        config: &scope.config().plugins,
        embedded: &[],
        running: version,
        blocked,
        profile_config: Some(scope.config()),
    }
}
