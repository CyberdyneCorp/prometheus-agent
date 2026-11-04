//! One bridged plugin's sidecar over time (spec: python-bridge, Sidecar
//! lifecycle; Call timeouts and stall detection; Crash isolation and
//! restart policy; Bridge diagnostics).
//!
//! The supervisor starts the sidecar on demand (or at once for plugins
//! with hooks, providers or computed sections), loads the plugin bounded
//! by `plugins.load_timeout_ms`, pings it every 15 seconds and kills it
//! after 3 unanswered pings, stops it after `idle_shutdown_secs` without
//! calls when nothing needs it running, and restarts it after a crash with
//! a backoff from 1 to 60 seconds. Five crashes within ten minutes mark
//! the plugin `failed` until `prometheus bridge restart <id>` (a request
//! file this process watches) or the next process start.
//!
//! Every change is written to `<profile>/run/bridge/<id>.json`, which
//! `prometheus bridge status` reads from any process.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pmt_config::schema::{PluginsConfig, PythonBridgeConfig};
use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::limits::Limits;
use crate::log::Log;
use crate::registrations::{self, Registrations};
use crate::sidecar::{CallError, HostHandler, Sidecar, Spawn};

/// The bounds and policy of every sidecar of a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// `python_bridge.call_timeout_ms`.
    pub call_timeout: Duration,
    /// `plugins.hook_timeout_ms`.
    pub hook_timeout: Duration,
    /// `plugins.load_timeout_ms`.
    pub load_timeout: Duration,
    /// `python_bridge.idle_shutdown_secs` (`None`: never).
    pub idle_shutdown: Option<Duration>,
    /// Ping period (15 s).
    pub ping_every: Duration,
    /// Unanswered pings that make it hung (3).
    pub pings_missed: u32,
    /// Memory and process bounds.
    pub limits: Limits,
    /// First restart delay (1 s), doubling up to `backoff_max` (60 s).
    pub backoff_start: Duration,
    /// Longest restart delay.
    pub backoff_max: Duration,
    /// The crash-loop window (10 min).
    pub crash_window: Duration,
    /// Crashes within the window that fail the plugin (5).
    pub crash_limit: usize,
}

impl Settings {
    /// The settings of a profile's configuration.
    #[must_use]
    pub fn from_config(bridge: &PythonBridgeConfig, plugins: &PluginsConfig) -> Self {
        Self {
            call_timeout: Duration::from_millis(bridge.call_timeout_ms),
            hook_timeout: Duration::from_millis(plugins.hook_timeout_ms),
            load_timeout: Duration::from_millis(plugins.load_timeout_ms),
            idle_shutdown: (bridge.idle_shutdown_secs > 0)
                .then(|| Duration::from_secs(bridge.idle_shutdown_secs)),
            ping_every: Duration::from_secs(15),
            pings_missed: 3,
            limits: Limits {
                max_memory_bytes: bridge.max_memory_bytes,
                max_processes: bridge.max_processes,
            },
            backoff_start: Duration::from_secs(1),
            backoff_max: Duration::from_mins(1),
            crash_window: Duration::from_mins(10),
            crash_limit: 5,
        }
    }
}

/// Builds the confined command of a sidecar (the embedder owns the
/// sandbox and the egress proxy).
pub trait Launcher: Send + Sync {
    /// A fresh command for one start.
    ///
    /// # Errors
    ///
    /// Why the sidecar cannot be launched (missing runtime, sandbox).
    fn command(&self) -> Result<Command, Error>;
}

/// One plugin as the supervisor runs it.
pub struct PluginSpec {
    /// The plugin id.
    pub id: String,
    /// The profile it runs for.
    pub profile: String,
    /// `<profile>` directory (status files).
    pub profile_dir: PathBuf,
    /// The `plugin.load` parameters.
    pub load: Value,
    /// How to start it.
    pub launcher: Arc<dyn Launcher>,
    /// Serves its `host.call` requests.
    pub host: Arc<dyn HostHandler>,
    /// Its log.
    pub log: Arc<Log>,
    /// Where its registrations are cached.
    pub cache: PathBuf,
}

/// Where a sidecar is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Not started yet, or stopped.
    Stopped,
    /// Starting and loading the plugin.
    Starting,
    /// Serving calls.
    Running,
    /// Stopped after `idle_shutdown_secs` without calls.
    Idle,
    /// Ended unexpectedly; a restart is scheduled.
    Crashed,
    /// Crashed too often, or failed to load.
    Failed,
}

impl State {
    /// The spelling `bridge status` shows.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Idle => "idle",
            Self::Crashed => "crashed",
            Self::Failed => "failed",
        }
    }
}

/// One row of `prometheus bridge status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The plugin.
    pub plugin: String,
    /// Its profile.
    pub profile: String,
    /// The sidecar's process id, while running.
    pub pid: Option<u32>,
    /// Its state.
    pub state: State,
    /// Unix seconds when the running sidecar started.
    pub started_at: Option<u64>,
    /// Restarts after crashes.
    pub restarts: u32,
    /// Resident bytes of its tree at the last sample.
    pub memory: u64,
    /// The process that supervises it.
    pub owner_pid: u32,
    /// The last failure.
    pub problem: Option<String>,
}

struct Inner {
    state: State,
    sidecar: Option<Sidecar>,
    registrations: Option<Registrations>,
    restarts: u32,
    crashes: VecDeque<Instant>,
    backoff: Duration,
    retry_at: Option<Instant>,
    generation: u64,
    last_used: Instant,
    started_at: Option<u64>,
    problem: Option<String>,
}

/// A plugin's sidecar supervisor.
pub struct Supervisor {
    spec: PluginSpec,
    settings: Settings,
    me: Weak<Supervisor>,
    inner: Mutex<Inner>,
    settled: Condvar,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor")
            .field("plugin", &self.spec.id)
            .finish_non_exhaustive()
    }
}

/// `plugin '<id>' is failed; see prometheus bridge logs <id>`.
#[must_use]
pub fn failed_text(plugin: &str) -> String {
    format!("plugin '{plugin}' is failed; see prometheus bridge logs {plugin}")
}

impl Supervisor {
    /// A supervisor with nothing started.
    #[must_use]
    pub fn new(spec: PluginSpec, settings: Settings) -> Arc<Self> {
        let registrations = registrations::read_cache(&spec.cache);
        let backoff = settings.backoff_start;
        Arc::new_cyclic(|me| Self {
            spec,
            settings,
            me: me.clone(),
            inner: Mutex::new(Inner {
                state: State::Stopped,
                sidecar: None,
                registrations,
                restarts: 0,
                crashes: VecDeque::new(),
                backoff,
                retry_at: None,
                generation: 0,
                last_used: Instant::now(),
                started_at: None,
                problem: None,
            }),
            settled: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The plugin id.
    #[must_use]
    pub fn plugin(&self) -> &str {
        &self.spec.id
    }

    /// Its settings.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// What the plugin registered at its last load (or the cache of an
    /// earlier process).
    #[must_use]
    pub fn registrations(&self) -> Option<Registrations> {
        self.lock().registrations.clone()
    }

    /// Starts the sidecar now when it is not running and returns what the
    /// plugin registered.
    ///
    /// # Errors
    ///
    /// The start or load failure (`bridge.call`), or the failed state.
    pub fn start_now(&self) -> Result<Registrations, Error> {
        self.ensure().map_err(CallError::into_error)?;
        self.registrations()
            .ok_or_else(|| Error::new(&codes::BRIDGE_CALL, "the plugin registered nothing"))
    }

    /// The running sidecar, started when needed.
    ///
    /// # Errors
    ///
    /// [`CallError::Crashed`] while failed or before a scheduled restart.
    pub fn ensure(&self) -> Result<Sidecar, CallError> {
        let mut inner = self.lock();
        loop {
            match inner.state {
                State::Running => match inner.sidecar.clone() {
                    Some(sidecar) => {
                        // Ended, and its exit not handled yet.
                        if let Some(status) = sidecar.ended() {
                            self.ended(&mut inner, &sidecar, &status);
                        } else {
                            inner.last_used = Instant::now();
                            return Ok(sidecar);
                        }
                    }
                    None => inner.state = State::Stopped,
                },
                State::Starting => {
                    inner = self
                        .settled
                        .wait(inner)
                        .unwrap_or_else(PoisonError::into_inner);
                }
                State::Failed if !self.restart_requested() => {
                    return Err(CallError::Crashed(failed_text(&self.spec.id)));
                }
                State::Failed => {
                    inner.crashes.clear();
                    inner.backoff = self.settings.backoff_start;
                    inner.state = State::Stopped;
                }
                State::Crashed if inner.retry_at.is_some_and(|at| Instant::now() < at) => {
                    let problem = inner.problem.clone().unwrap_or_default();
                    return Err(CallError::Crashed(problem));
                }
                State::Stopped | State::Idle | State::Crashed => {
                    inner.state = State::Starting;
                    drop(inner);
                    let started = self.start();
                    inner = self.lock();
                    self.settle(&mut inner, started);
                    self.settled.notify_all();
                    if inner.state != State::Running {
                        let problem = inner.problem.clone().unwrap_or_default();
                        return Err(CallError::Crashed(problem));
                    }
                }
            }
        }
    }

    /// A `bridge restart` request file was written for this plugin.
    fn restart_requested(&self) -> bool {
        let path = restart_request(&self.spec.profile_dir, &self.spec.id);
        let requested = path.exists();
        if requested {
            let _ = std::fs::remove_file(path);
        }
        requested
    }

    fn start(&self) -> Result<(Sidecar, Registrations, u64), String> {
        let generation = {
            let mut inner = self.lock();
            inner.generation += 1;
            inner.generation
        };
        let command = self
            .spec
            .launcher
            .command()
            .map_err(|e| e.message().to_owned())?;
        let me = self.me.clone();
        let spawn = Spawn {
            command,
            plugin: self.spec.id.clone(),
            log: Arc::clone(&self.spec.log),
            limits: self.settings.limits,
            host: Arc::clone(&self.spec.host),
            on_exit: Box::new(move |status| {
                if let Some(me) = me.upgrade() {
                    me.exited(generation, &status);
                }
            }),
        };
        let level = pmt_plugins::python::api::HERMES_COMPAT_LEVEL;
        let sidecar = Sidecar::start(spawn, level).map_err(|e| e.message().to_owned())?;
        match sidecar.call("plugin.load", &self.spec.load, self.settings.load_timeout) {
            Ok(answer) => {
                let registrations: Registrations = serde_json::from_value(answer).map_err(|e| {
                    format!("plugin '{}': unreadable registrations: {e}", self.spec.id)
                })?;
                Ok((sidecar, registrations, generation))
            }
            Err(err) => {
                sidecar.shutdown();
                Err(match err {
                    CallError::Timeout(bound) => {
                        format!("load timed out after {} ms", bound.as_millis())
                    }
                    other => other.message(),
                })
            }
        }
    }

    fn settle(&self, inner: &mut Inner, started: Result<(Sidecar, Registrations, u64), String>) {
        match started {
            Ok((sidecar, registrations, generation)) => {
                registrations::write_cache(&self.spec.cache, &registrations);
                inner.registrations = Some(registrations);
                inner.sidecar = Some(sidecar);
                inner.state = State::Running;
                inner.last_used = Instant::now();
                inner.started_at = Some(unix_now());
                inner.problem = None;
                self.watch(generation);
            }
            Err(problem) => {
                self.spec.log.note(&format!("load failed: {problem}"));
                inner.state = State::Failed;
                inner.problem = Some(problem);
            }
        }
        self.publish(inner);
    }

    /// The sidecar of `generation` ended with `status`.
    fn exited(&self, generation: u64, status: &str) {
        let mut inner = self.lock();
        if inner.generation != generation || inner.state != State::Running {
            return;
        }
        if let Some(sidecar) = inner.sidecar.clone() {
            self.ended(&mut inner, &sidecar, status);
        }
    }

    /// Handles the end of the running `sidecar`: a deliberate stop, or a
    /// crash that schedules a restart or fails the plugin.
    fn ended(&self, inner: &mut Inner, sidecar: &Sidecar, status: &str) {
        inner.sidecar = None;
        inner.started_at = None;
        if sidecar.stopping() {
            inner.state = State::Stopped;
            self.publish(inner);
            return;
        }
        let now = Instant::now();
        inner.crashes.push_back(now);
        while inner
            .crashes
            .front()
            .is_some_and(|t| now.duration_since(*t) > self.settings.crash_window)
        {
            inner.crashes.pop_front();
        }
        inner.problem = Some(crate::sidecar::crash_text(&self.spec.id, status));
        if inner.crashes.len() >= self.settings.crash_limit {
            inner.state = State::Failed;
            self.spec.log.note(&format!(
                "failed after {} crashes within {} s; run prometheus bridge restart {}",
                inner.crashes.len(),
                self.settings.crash_window.as_secs(),
                self.spec.id
            ));
            self.publish(inner);
            return;
        }
        let delay = inner.backoff;
        inner.backoff = (inner.backoff * 2).min(self.settings.backoff_max);
        inner.retry_at = Some(now + delay);
        inner.state = State::Crashed;
        inner.restarts += 1;
        self.publish(inner);
        let me = self.me.clone();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            if let Some(me) = me.upgrade()
                && me.lock().state == State::Crashed
            {
                let _ = me.ensure();
            }
        });
    }

    /// Pings the sidecar of `generation` and stops it when idle.
    fn watch(&self, generation: u64) {
        let me = self.me.clone();
        let every = self.settings.ping_every;
        std::thread::spawn(move || {
            let mut missed = 0;
            loop {
                std::thread::sleep(every);
                let Some(me) = me.upgrade() else { return };
                if !me.tick(generation, &mut missed) {
                    return;
                }
            }
        });
    }

    /// One ping period of the sidecar of `generation`: stop it when idle,
    /// kill it after the missed pings; `false` ends the watch.
    fn tick(&self, generation: u64, missed: &mut u32) -> bool {
        let Some(sidecar) = self.current(generation) else {
            return false;
        };
        if self.idle_due() {
            self.stop_as(State::Idle);
            return false;
        }
        if sidecar.ping(self.settings.ping_every) {
            *missed = 0;
            self.publish(&self.lock());
            return true;
        }
        *missed += 1;
        if *missed < self.settings.pings_missed {
            return true;
        }
        let reason = format!("unresponsive: {missed} pings unanswered");
        self.spec.log.note(&reason);
        sidecar.kill(&reason);
        false
    }

    fn current(&self, generation: u64) -> Option<Sidecar> {
        let inner = self.lock();
        (inner.generation == generation && inner.state == State::Running)
            .then(|| inner.sidecar.clone())
            .flatten()
    }

    fn idle_due(&self) -> bool {
        let inner = self.lock();
        let lazy = inner.registrations.as_ref().is_some_and(|r| !r.eager());
        lazy && self
            .settings
            .idle_shutdown
            .is_some_and(|idle| inner.last_used.elapsed() >= idle)
    }

    /// Calls `method` on the (started) sidecar.
    ///
    /// # Errors
    ///
    /// [`CallError`].
    pub fn call(
        &self,
        method: &str,
        params: &Value,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        let sidecar = self.ensure()?;
        let answer = sidecar.call(method, params, timeout);
        self.lock().last_used = Instant::now();
        answer
    }

    /// Runs tool `name`.
    ///
    /// # Errors
    ///
    /// [`CallError`]; `tool '<name>' is no longer provided by plugin
    /// '<id>'` when the plugin's current registrations lack it.
    pub fn tool(&self, name: &str, args: &Value, context: &Value) -> Result<String, CallError> {
        self.ensure()?;
        if !self.registrations().is_some_and(|r| r.has_tool(name)) {
            return Err(CallError::Remote {
                code: "tool_gone".to_owned(),
                message: format!(
                    "tool '{name}' is no longer provided by plugin '{}'",
                    self.spec.id
                ),
            });
        }
        let params = json!({"name": name, "args": args, "context": context});
        let answer = self.call("tool.invoke", &params, self.settings.call_timeout)?;
        Ok(answer
            .as_str()
            .map_or_else(|| answer.to_string(), str::to_owned))
    }

    /// Stops the sidecar (`bridge.shutdown`, then terminate).
    pub fn stop(&self) {
        self.stop_as(State::Stopped);
    }

    fn stop_as(&self, state: State) {
        let sidecar = {
            let mut inner = self.lock();
            inner.state = state;
            inner.started_at = None;
            inner.sidecar.take()
        };
        if let Some(sidecar) = sidecar {
            sidecar.shutdown();
        }
        self.publish(&self.lock());
    }

    /// Clears a failed state and starts again (`bridge restart`).
    ///
    /// # Errors
    ///
    /// The start failure.
    pub fn restart(&self) -> Result<(), Error> {
        self.stop();
        {
            let mut inner = self.lock();
            inner.crashes.clear();
            inner.backoff = self.settings.backoff_start;
            inner.retry_at = None;
            inner.problem = None;
        }
        self.ensure().map(|_| ()).map_err(CallError::into_error)
    }

    /// Its status row.
    #[must_use]
    pub fn status(&self) -> Status {
        self.status_of(&self.lock())
    }

    fn status_of(&self, inner: &Inner) -> Status {
        Status {
            plugin: self.spec.id.clone(),
            profile: self.spec.profile.clone(),
            pid: inner.sidecar.as_ref().map(Sidecar::pid),
            state: inner.state,
            started_at: inner.started_at,
            restarts: inner.restarts,
            memory: inner.sidecar.as_ref().map_or(0, Sidecar::memory),
            owner_pid: std::process::id(),
            problem: inner.problem.clone(),
        }
    }

    fn publish(&self, inner: &Inner) {
        let path = status_path(&self.spec.profile_dir, &self.spec.id);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(bytes) = serde_json::to_vec(&self.status_of(inner)) {
            let part = path.with_extension("json.part");
            if std::fs::write(&part, bytes).is_ok() {
                let _ = std::fs::rename(part, path);
            }
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        let sidecar = self
            .inner
            .get_mut()
            .map(|i| i.sidecar.take())
            .ok()
            .flatten();
        if let Some(sidecar) = sidecar {
            sidecar.shutdown();
        }
        let _ = std::fs::remove_file(status_path(&self.spec.profile_dir, &self.spec.id));
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `<profile>/run/bridge/<plugin>.json`.
#[must_use]
pub fn status_path(profile_dir: &std::path::Path, plugin: &str) -> PathBuf {
    profile_dir
        .join("run")
        .join("bridge")
        .join(format!("{plugin}.json"))
}

/// `<profile>/run/bridge/<plugin>.restart`: written by `bridge restart`.
#[must_use]
pub fn restart_request(profile_dir: &std::path::Path, plugin: &str) -> PathBuf {
    profile_dir
        .join("run")
        .join("bridge")
        .join(format!("{plugin}.restart"))
}

/// Every published status of a profile.
#[must_use]
pub fn published(profile_dir: &std::path::Path) -> Vec<Status> {
    let Ok(entries) = std::fs::read_dir(profile_dir.join("run").join("bridge")) else {
        return Vec::new();
    };
    let mut rows: Vec<Status> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
        .collect();
    rows.sort_by(|a, b| a.plugin.cmp(&b.plugin));
    rows
}

/// The published rows whose supervising process is still alive.
#[must_use]
pub fn live(profile_dir: &std::path::Path) -> Vec<Status> {
    let mut system = sysinfo::System::new();
    published(profile_dir)
        .into_iter()
        .filter(|row| {
            let pid = sysinfo::Pid::from_u32(row.owner_pid);
            system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
            system.process(pid).is_some()
        })
        .collect()
}
