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

pub mod catalog;
pub mod llm;
mod ports;
pub mod reload;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use pmt_agent::turn::TurnHooks;
use pmt_agent::turn::model::BoxFuture;
use pmt_config::ProfileScope;
use pmt_plugins::hooks::{HookSettings, HookSource, Hooks, PluginToolHooks};
use pmt_plugins::host::{CallInfo, Runtime};
use pmt_plugins::load::{LoadInputs, Loaded, State, Status, load, survey};
use pmt_tools::dispatch::ToolHooks;
use semver::Version;

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

    /// The turn loop's hook seam for one session.
    #[must_use]
    pub fn turn_hooks(&self, call: CallInfo) -> Arc<dyn TurnHooks> {
        Arc::new(SessionTurnHooks {
            handle: Arc::clone(&self.handle),
            call,
        })
    }
}

struct SessionTurnHooks {
    handle: Arc<Handle>,
    call: CallInfo,
}

impl TurnHooks for SessionTurnHooks {
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

/// Every profile's live set in this process, by profile directory.
fn handles() -> &'static Mutex<HashMap<PathBuf, Arc<Handle>>> {
    static HANDLES: OnceLock<Mutex<HashMap<PathBuf, Arc<Handle>>>> = OnceLock::new();
    HANDLES.get_or_init(Mutex::default)
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
            loaded,
        });
    }
    ProfilePlugins { handle }
}

/// Loads when some plugin is to be instantiated; otherwise reports the
/// survey without starting the WASM engine.
fn load_now(scope: &ProfileScope, inputs: &LoadInputs<'_>, statuses: Vec<Status>) -> Loaded {
    let settings = hook_settings(scope);
    if !statuses.iter().any(|s| s.state == State::Loaded) {
        return Loaded::empty(statuses, settings);
    }
    match runtime() {
        Some(runtime) => load(runtime, &ports::ports(scope), inputs),
        None => Loaded::empty(engine_unavailable(statuses), settings),
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
    let mut text = format!("{grants:?}|{:?}|", scope.config().plugins);
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
    }
}
