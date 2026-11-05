//! The `local` terminal backend: one session's commands on this host under
//! the OS sandbox (spec: execution-environments, Local backend runs under
//! the OS sandbox; Working directory resolution; Session cwd tracking;
//! Terminal argument validation; Foreground cap, promotion and server
//! guard; Pre-execution guards fail closed; Foreground result shaping;
//! Infrastructure failures are degraded results; Shell startup files).
//!
//! A call goes: argument validation, foreground/background planning
//! (trailing `&`, server guard, promotion past
//! `terminal.max_foreground_secs`), the guard chain (capability, approval,
//! workdir) under its deadline, environment preparation (the shell init
//! step, once), then the run. Nothing executes before every step passed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use pmt_config::schema::{DegradedMode, TerminalConfig};
use pmt_core::secret::{Redactor, SecretString};
use tokio::sync::mpsc;

use super::args::TerminalArgs;
use super::classify::{Plan, classify};
use super::cwd::expand_home;
use super::env::ChildEnv;
use super::guard::{FnGuard, Guard, GuardChain, GuardRequest, Stop, WorkdirGuard};
use super::process::{self, Clock, Completion, ProcessRegistry};
use super::result::{Status, TerminalResult};
use super::shape::{self, OutputHook};
use super::shell::{
    INIT_TIMEOUT_HINT, RunScript, Shell, ShellKind, StateFiles, init_argv, run_argv,
};
use super::spawn::{self, GroupGuard, SpawnError};
use super::spill::{self, Capture};
use super::target::Target;

/// How long output readers may lag behind a foreground exit.
const READ_GRACE: Duration = Duration::from_millis(500);

/// Everything a session's local backend is built from.
pub struct TerminalSetup {
    /// The session's `[terminal]` settings.
    pub terminal: TerminalConfig,
    /// `tool_output.max_chars`.
    pub max_chars: crate::setting::Limit,
    /// `tools.kill_grace_secs`.
    pub kill_grace: Duration,
    /// Where commands run: [`super::target::LocalTarget`] under the
    /// session sandbox, or a container/SSH [`super::remote::RemoteTarget`].
    pub target: Arc<dyn Target>,
    /// The child environment (see [`ChildEnv::build`]).
    pub env: ChildEnv,
    /// The shell commands run in (a remote target picks its own).
    pub shell: Shell,
    /// The session scratch directory on this host (granted to the sandbox).
    pub scratch: PathBuf,
    /// `<profile>/cache/terminal-spill/<session_id>` ([`spill::session_dir`]).
    pub spill_dir: PathBuf,
    /// Where the session starts ([`super::cwd::resolve`]).
    pub start_cwd: PathBuf,
    /// The user's home (for `~` and `~/.bashrc`).
    pub user_home: Option<PathBuf>,
    /// Scrubs every output (the profile's redactor).
    pub redactor: Redactor,
    /// `transform_terminal_output` hooks.
    pub hooks: Vec<OutputHook>,
    /// Guards between the capability check and workdir validation (the
    /// approval gate).
    pub guards: GuardChain,
    /// One-time notices for the first result (for example from
    /// [`super::cwd::resolve`]).
    pub notices: Vec<String>,
    /// Where background completion notifications go.
    pub notify: Option<mpsc::UnboundedSender<Completion>>,
    /// The time source (default: the system clock).
    pub clock: Option<Clock>,
}

impl std::fmt::Debug for TerminalSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalSetup")
            .field("shell", &self.shell)
            .field("scratch", &self.scratch)
            .field("start_cwd", &self.start_cwd)
            .finish_non_exhaustive()
    }
}

/// The state of the shell environment (the init snapshot).
#[derive(Debug, Default)]
struct EnvState {
    ready: bool,
    /// The environment failed and was evicted at least once.
    evicted: bool,
}

/// What [`Terminal::prepare`] hands to a run.
struct Prepared {
    recreated: bool,
}

/// A planned execution.
enum Exec {
    Foreground(String),
    Background {
        command: String,
        notify: bool,
        promoted: bool,
    },
}

/// How a background command came to be, for its result.
#[derive(Debug, Clone, Copy)]
struct Started {
    /// Promoted past `terminal.max_foreground_secs`.
    promoted: bool,
    /// Asked for a pty (not supported yet).
    pty: bool,
}

/// One session on the local backend.
pub struct Terminal {
    setup: TerminalSetup,
    chain: GuardChain,
    activity: Arc<Activity>,
    cwd: Mutex<PathBuf>,
    env_state: tokio::sync::Mutex<EnvState>,
    pending: Mutex<Pending>,
    processes: ProcessRegistry,
    runs: AtomicU64,
    secret_source: std::sync::RwLock<Option<SecretSource>>,
    path_source: std::sync::RwLock<Option<PathSource>>,
}

/// Secrets added to every command's environment at spawn time (for
/// example the declared secrets of the skills a session loaded), resolved
/// per command so a rotated value is used at once.
pub type SecretSource = Arc<dyn Fn() -> Vec<(String, SecretString)> + Send + Sync>;

/// Directories put first on `PATH` for one command, from its text (the
/// pinned Python environment of a skill whose script it runs).
pub type PathSource = Arc<dyn Fn(&str) -> Vec<PathBuf> + Send + Sync>;

/// Things the next result carries once.
#[derive(Debug, Default)]
struct Pending {
    notices: Vec<String>,
    hint: Option<String>,
}

impl std::fmt::Debug for Terminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Terminal")
            .field("setup", &self.setup)
            .field("processes", &self.processes)
            .finish_non_exhaustive()
    }
}

impl Terminal {
    /// A session backend. Creates the scratch directory and prunes spill
    /// files older than seven days.
    ///
    /// # Errors
    ///
    /// The scratch directory cannot be created.
    pub fn new(setup: TerminalSetup) -> std::io::Result<Self> {
        pmt_core::private_fs::create_dir(&setup.scratch)?;
        if let Some(root) = setup.spill_dir.parent() {
            spill::prune(root, std::time::SystemTime::now());
        }
        let target = Arc::clone(&setup.target);
        let capability = FnGuard::new("capability", move |_: &GuardRequest| {
            target.check().map_err(Stop::Denied)
        });
        let mut guards: Vec<Arc<dyn Guard>> = vec![Arc::new(capability)];
        guards.extend(setup.guards.guards().iter().cloned());
        if setup.target.descriptor().uses_host_filesystem {
            // A remote script changes directory itself (`cd || exit 126`).
            guards.push(Arc::new(WorkdirGuard));
        }
        let chain = GuardChain::new(guards);
        let mut notices = setup.notices.clone();
        notices.extend(setup.shell.notice.clone());
        if setup.shell.kind == ShellKind::Pwsh || setup.shell.kind == ShellKind::PowerShell {
            notices.push(
                "persistent shell state (exports, functions, cd) does not carry over between PowerShell commands"
                    .to_owned(),
            );
        }
        let processes = ProcessRegistry::new(process::Options {
            kill_grace: setup.kill_grace,
            max_chars: Arc::clone(&setup.max_chars),
            redactor: setup.redactor.clone(),
            notify: setup.notify.clone(),
            clock: setup
                .clock
                .clone()
                .unwrap_or_else(|| Arc::new(Instant::now)),
        });
        Ok(Self {
            activity: Arc::new(Activity::default()),
            cwd: Mutex::new(setup.start_cwd.clone()),
            chain,
            env_state: tokio::sync::Mutex::default(),
            pending: Mutex::new(Pending {
                notices,
                hint: None,
            }),
            processes,
            runs: AtomicU64::new(0),
            secret_source: std::sync::RwLock::new(None),
            path_source: std::sync::RwLock::new(None),
            setup,
        })
    }

    /// Adds `source`'s secrets to the environment of every later command.
    pub fn set_secret_source(&self, source: SecretSource) {
        *self
            .secret_source
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(source);
    }

    /// Puts `source`'s directories first on `PATH` of every later command
    /// it names them for.
    pub fn set_path_source(&self, source: PathSource) {
        *self
            .path_source
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(source);
    }

    /// The environment of `command`: the session's plus the secret
    /// source's, with the path source's directories first on `PATH`.
    fn command_env(&self, command: &str) -> std::borrow::Cow<'_, ChildEnv> {
        let source = self
            .secret_source
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let paths = self
            .path_source
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .map(|p| p(command))
            .unwrap_or_default();
        let mut env = match source.map(|s| s()).filter(|secrets| !secrets.is_empty()) {
            None if paths.is_empty() => return std::borrow::Cow::Borrowed(&self.setup.env),
            None => self.setup.env.clone(),
            Some(secrets) => self.setup.env.with_secrets(secrets),
        };
        env.prepend_path(&paths);
        std::borrow::Cow::Owned(env)
    }

    /// The session's current directory.
    #[must_use]
    pub fn cwd(&self) -> PathBuf {
        self.cwd
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The session's background processes (the `process` tool).
    #[must_use]
    pub fn processes(&self) -> &ProcessRegistry {
        &self.processes
    }

    /// Where commands run.
    #[must_use]
    pub fn target(&self) -> &Arc<dyn Target> {
        &self.setup.target
    }

    /// Session end: stops every background process group, then releases
    /// the backend environment.
    pub async fn close(&self) {
        self.processes.close().await;
        let target = Arc::clone(&self.setup.target);
        let _ = tokio::task::spawn_blocking(move || target.close()).await;
    }

    /// The shell in use: the remote backend's own, else the configured one.
    fn shell(&self) -> Shell {
        self.setup
            .target
            .shell()
            .unwrap_or_else(|| self.setup.shell.clone())
    }

    /// Where `~` points: the remote home, else this host's.
    fn home(&self) -> Option<PathBuf> {
        self.setup
            .target
            .home()
            .or_else(|| self.setup.user_home.clone())
    }

    fn state_files(&self) -> StateFiles {
        StateFiles::in_dir(&self.setup.target.state_dir())
    }

    /// Runs a blocking target call off the async runtime.
    async fn blocking<R: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn Target) -> R + Send + 'static,
    ) -> R {
        let target = Arc::clone(&self.setup.target);
        match tokio::task::spawn_blocking(move || f(target.as_ref())).await {
            Ok(value) => value,
            Err(err) => std::panic::resume_unwind(err.into_panic()),
        }
    }

    /// Runs one `terminal` call.
    pub async fn run(&self, args: TerminalArgs) -> TerminalResult {
        let busy = self.activity.enter();
        let mut result = self.dispatch(args).await;
        drop(busy);
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        result.notices.splice(0..0, pending.notices.drain(..));
        if result.status != Status::Degraded
            && let Some(hint) = pending.hint.take()
        {
            result.hint = Some(match result.hint.take() {
                Some(other) => format!("{hint}. {other}"),
                None => hint,
            });
        }
        result
    }

    async fn dispatch(&self, args: TerminalArgs) -> TerminalResult {
        if let Err(message) = args.validate() {
            return TerminalResult::refused(message);
        }
        // Validated positive; whole seconds, rounded up.
        let timeout_secs = args.timeout.map_or(self.setup.terminal.timeout_secs, |t| {
            Duration::try_from_secs_f64(t.ceil()).map_or(u64::MAX, |d| d.as_secs())
        });
        let exec = match self.plan(&args, timeout_secs) {
            Ok(exec) => exec,
            Err(message) => return TerminalResult::refused(message),
        };
        let cwd = self.call_cwd(args.workdir.as_deref());
        let request = GuardRequest {
            command: args.command.clone(),
            cwd: cwd.clone(),
            background: matches!(exec, Exec::Background { .. }),
        };
        if let Err(message) = self.chain.run(&request, timeout_secs).await {
            return TerminalResult::refused(message);
        }
        if let Some(plugin) = self.setup.target.plugin() {
            return match exec {
                Exec::Foreground(command) => {
                    self.on_plugin(
                        &plugin,
                        &command,
                        &cwd,
                        timeout_secs,
                        args.workdir.is_none(),
                    )
                    .await
                }
                Exec::Background {
                    command,
                    notify,
                    promoted,
                } => {
                    let bound = args.timeout.map(|_| timeout_secs);
                    let started = Started {
                        promoted,
                        pty: args.pty,
                    };
                    self.on_plugin_background(&plugin, &command, &cwd, notify, bound, started)
                }
            };
        }
        let prepared = match self.prepare().await {
            Ok(prepared) => prepared,
            Err(SpawnError::Refused(err)) => return TerminalResult::refused(err.message()),
            Err(SpawnError::Infrastructure(reason)) => return self.degraded(reason).await,
        };
        // Connecting resolves a remote `~`.
        let cwd = self.call_cwd(args.workdir.as_deref());
        let mut result = match exec {
            Exec::Foreground(command) => {
                self.foreground(&command, &cwd, timeout_secs, args.workdir.is_none())
                    .await
            }
            Exec::Background {
                command,
                notify,
                promoted,
            } => {
                let started = Started {
                    promoted,
                    pty: args.pty,
                };
                self.background(&command, &cwd, notify, started).await
            }
        };
        result.environment_recreated |= prepared.recreated;
        result
    }

    /// A foreground command on a plugin backend.
    async fn on_plugin(
        &self,
        plugin: &Arc<dyn pmt_sandbox::backend::RemoteBackend>,
        command: &str,
        cwd: &Path,
        timeout_secs: u64,
        track: bool,
    ) -> TerminalResult {
        let env = self.command_env(command);
        let shaping = super::plugin::Shaping {
            spill_dir: &self.setup.spill_dir,
            max_chars: (self.setup.max_chars)(),
            hooks: &self.setup.hooks,
            redactor: &self.setup.redactor,
        };
        let start = cwd.to_string_lossy();
        match super::plugin::run(plugin, command, &start, &env, timeout_secs, shaping).await {
            super::plugin::Outcome::Failed(reason) => self.degraded(reason).await,
            super::plugin::Outcome::Ran(result, observed) => {
                let mut result = *result;
                if let Some(observed) = observed.filter(|o| !o.is_empty()) {
                    if observed != start {
                        result.cwd = Some(observed.clone());
                    }
                    if track {
                        *self.cwd.lock().unwrap_or_else(PoisonError::into_inner) =
                            PathBuf::from(observed);
                    }
                }
                result
            }
        }
    }

    /// A background command on a plugin backend: the plugin's `execute`
    /// runs as a tracked job, bounded only when the call gave a timeout.
    fn on_plugin_background(
        &self,
        plugin: &Arc<dyn pmt_sandbox::backend::RemoteBackend>,
        command: &str,
        cwd: &Path,
        notify: bool,
        timeout_secs: Option<u64>,
        started: Started,
    ) -> TerminalResult {
        if let Err(message) = self.processes.reserve() {
            return TerminalResult::refused(message);
        }
        let env = self.command_env(command);
        let job = super::plugin::job(plugin, command, &cwd.to_string_lossy(), &env, timeout_secs);
        let id = self.processes.track_job(job, command, notify);
        self.started(id, started)
    }

    fn plan(&self, args: &TerminalArgs, timeout_secs: u64) -> Result<Exec, String> {
        if args.background {
            return Ok(Exec::Background {
                command: args.command.clone(),
                notify: args.notify_on_complete,
                promoted: false,
            });
        }
        let command = match classify(&args.command) {
            Plan::Refuse(message) => return Err(message),
            Plan::Background { command } => {
                return Ok(Exec::Background {
                    command,
                    notify: false,
                    promoted: false,
                });
            }
            Plan::Foreground => args.command.clone(),
        };
        if timeout_secs > self.setup.terminal.max_foreground_secs {
            return Ok(Exec::Background {
                command,
                notify: true,
                promoted: true,
            });
        }
        Ok(Exec::Foreground(command))
    }

    /// The call's directory: `workdir` (`~` expanded, relative to the
    /// session cwd) or the session cwd.
    fn call_cwd(&self, workdir: Option<&str>) -> PathBuf {
        let session = self.cwd();
        match workdir {
            Some(dir) => session.join(expand_home(dir, self.home().as_deref())),
            None => session,
        }
    }

    /// Builds the shell environment once (and again after an eviction).
    async fn prepare(&self) -> Result<Prepared, SpawnError> {
        let mut state = self.env_state.lock().await;
        if state.ready {
            return Ok(Prepared { recreated: false });
        }
        self.blocking(|target| target.connect()).await?;
        if let Some(home) = self.setup.target.home() {
            // A remote session starts in `~` (or below it) of the remote.
            let mut cwd = self.cwd.lock().unwrap_or_else(PoisonError::into_inner);
            *cwd = expand_home(&cwd.to_string_lossy(), Some(&home));
        }
        if self.shell().kind.is_posix() {
            self.init_shell().await?;
        }
        state.ready = true;
        Ok(Prepared {
            recreated: state.evicted,
        })
    }

    async fn init_files(&self) -> Vec<PathBuf> {
        let home = self.home();
        let mut candidates = Vec::new();
        if self.shell().kind == ShellKind::Bash
            && self.setup.terminal.auto_source_bashrc
            && let Some(home) = &home
        {
            candidates.push((None, home.join(".bashrc")));
        }
        for configured in &self.setup.terminal.shell_init_files {
            candidates.push((
                Some(configured.clone()),
                expand_home(configured, home.as_deref()),
            ));
        }
        let checked = self
            .blocking(move |target| {
                candidates
                    .into_iter()
                    .map(|(configured, path)| (target.is_file(&path), configured, path))
                    .collect::<Vec<_>>()
            })
            .await;
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let mut files = Vec::new();
        for (exists, configured, path) in checked {
            match (exists, configured) {
                (true, _) => files.push(path),
                (false, Some(configured)) => pending.notices.push(format!(
                    "terminal.shell_init_files: '{configured}' not found; skipped"
                )),
                (false, None) => {}
            }
        }
        files
    }

    /// Sources the init files into the snapshot under
    /// `terminal.shell_init_timeout_secs`; on timeout the shell runs without
    /// them and the next result carries [`INIT_TIMEOUT_HINT`].
    async fn init_shell(&self) -> Result<(), SpawnError> {
        let state = self.state_files();
        let snapshot = state.snapshot.clone();
        self.blocking(move |target| target.remove(&snapshot)).await;
        let files = self.init_files().await;
        if files.is_empty() {
            return Ok(());
        }
        let argv = init_argv(&self.shell(), &state, self.setup.env.secret_names(), &files);
        let spawned =
            self.setup
                .target
                .spawn(&self.setup.env, &argv, &self.setup.scratch, false)?;
        let (mut child, group) = (spawned.child, spawned.group);
        let mut guard = GroupGuard::new(group.clone(), self.setup.kill_grace);
        // Drain (and discard) init output so a chatty rc file cannot block.
        let drains = [
            child
                .stdout
                .take()
                .map(|r| tokio::spawn(spawn::pump(r, |_| {}))),
            child
                .stderr
                .take()
                .map(|r| tokio::spawn(spawn::pump(r, |_| {}))),
        ];
        let limit = Duration::from_secs(self.setup.terminal.shell_init_timeout_secs);
        if tokio::time::timeout(limit, child.wait()).await.is_err() {
            spawn::stop(&mut child, &group, self.setup.kill_grace).await;
            let snapshot = state.snapshot.clone();
            self.blocking(move |target| target.remove(&snapshot)).await;
            self.pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .hint = Some(INIT_TIMEOUT_HINT.to_owned());
            tracing::warn!(
                "shell init timed out after {}s; running without init files",
                limit.as_secs()
            );
        }
        group.kill();
        guard.disarm();
        for drain in drains.into_iter().flatten() {
            drain.abort();
        }
        Ok(())
    }

    async fn foreground(
        &self,
        command: &str,
        cwd: &Path,
        timeout_secs: u64,
        track: bool,
    ) -> TerminalResult {
        let run = self.runs.fetch_add(1, Ordering::Relaxed);
        let state = self.state_files();
        let files = StateFiles {
            cwd: self
                .setup
                .target
                .state_dir()
                .join(format!("pmt-shell-cwd-{run}")),
            snapshot: state.snapshot,
        };
        let env = self.command_env(command);
        let shell = self.shell();
        let script = RunScript {
            command,
            cwd,
            use_snapshot: shell.kind.is_posix(),
            persist: self.setup.terminal.persistent_shell,
            track_cwd: true,
            hidden: env.secret_names(),
        };
        let argv = run_argv(&shell, &files, &script);
        let spawned = match self
            .setup
            .target
            .spawn(&env, &argv, &self.spawn_dir(cwd), false)
        {
            Ok(spawned) => spawned,
            Err(SpawnError::Refused(err)) => return TerminalResult::refused(err.message()),
            Err(SpawnError::Infrastructure(reason)) => return self.degraded(reason).await,
        };
        let (status, capture) = self.wait_foreground(spawned, timeout_secs).await;
        let shaped = capture
            .finish(
                (self.setup.max_chars)(),
                &self.setup.hooks,
                &self.setup.redactor,
            )
            .await;
        let mut result = TerminalResult::new(Status::Ok);
        result.output = shaped.output;
        result.truncated = shaped.truncated;
        result.full_output_path = shaped
            .full_output_path
            .map(|p| p.to_string_lossy().into_owned());
        if let Some(code) = status {
            if let Some(reason) = self.blocking(move |target| target.failure(code)).await {
                return self.degraded(reason).await;
            }
            result.exit_code = Some(code);
            result.status = if code == 0 { Status::Ok } else { Status::Error };
            result.exit_code_meaning = shape::exit_code_meaning(command, code);
            self.observe_cwd(&files.cwd, cwd, track, &mut result).await;
        } else {
            result.exit_code = Some(124);
            result.status = Status::Timeout;
            result.hint = Some(format!(
                "the command was stopped after {timeout_secs}s; pass a larger timeout, or background=true for long-running work"
            ));
        }
        let leftover = files.cwd.clone();
        self.blocking(move |target| target.remove(&leftover)).await;
        result
    }

    /// The host directory a client process starts in: the call's cwd for
    /// the local backend, the scratch directory for a remote one (whose
    /// script changes directory itself).
    fn spawn_dir(&self, cwd: &Path) -> PathBuf {
        if self.setup.target.descriptor().uses_host_filesystem {
            cwd.to_path_buf()
        } else {
            self.setup.scratch.clone()
        }
    }

    /// Waits for a foreground command (stopping its group at the timeout)
    /// while capturing its output. `None` means it timed out.
    async fn wait_foreground(
        &self,
        spawned: spawn::Spawned,
        timeout_secs: u64,
    ) -> (Option<i32>, Capture) {
        let (mut child, group) = (spawned.child, spawned.group);
        let mut guard = GroupGuard::new(group.clone(), self.setup.kill_grace);
        let capture = Arc::new(Mutex::new(Capture::new(&self.setup.spill_dir)));
        let readers: Vec<_> = [
            child.stdout.take().map(|r| capture_into(&capture, r)),
            child.stderr.take().map(|r| capture_into(&capture, r)),
        ]
        .into_iter()
        .flatten()
        .collect();
        let limit = Duration::from_secs(timeout_secs);
        let status = if let Ok(status) = tokio::time::timeout(limit, child.wait()).await {
            Some(status.map_or(-1, spawn::exit_code))
        } else {
            spawn::stop(&mut child, &group, self.setup.kill_grace).await;
            None
        };
        for reader in readers {
            let abort = reader.abort_handle();
            if tokio::time::timeout(READ_GRACE, reader).await.is_err() {
                abort.abort();
            }
        }
        // A foreground command leaves nothing running (background work is
        // explicit), then the group id is released.
        group.kill();
        guard.disarm();
        // Readers are done or aborted; take the capture out either way.
        let capture = std::mem::replace(
            &mut *capture.lock().unwrap_or_else(PoisonError::into_inner),
            Capture::new(&self.setup.spill_dir),
        );
        (status, capture)
    }

    /// Reads the directory the command finished in (`cwd_observed`): it is
    /// reported when it differs from `start` and becomes the session cwd
    /// unless the call had its own `workdir`.
    async fn observe_cwd(
        &self,
        file: &Path,
        start: &Path,
        track: bool,
        result: &mut TerminalResult,
    ) {
        let file = file.to_path_buf();
        let Some(text) = self.blocking(move |target| target.take_text(&file)).await else {
            return;
        };
        let observed = observed_path(
            text.trim_end_matches(['\r', '\n'])
                .trim_start_matches('\u{feff}'),
        );
        if observed.as_os_str().is_empty() {
            return;
        }
        let host = self.setup.target.descriptor().uses_host_filesystem;
        if !(observed == start || host && same_dir(&observed, start)) {
            result.cwd = Some(observed.to_string_lossy().into_owned());
        }
        if track {
            *self.cwd.lock().unwrap_or_else(PoisonError::into_inner) = observed;
        }
    }

    async fn background(
        &self,
        command: &str,
        cwd: &Path,
        notify: bool,
        started: Started,
    ) -> TerminalResult {
        if let Err(message) = self.processes.reserve() {
            return TerminalResult::refused(message);
        }
        let env = self.command_env(command);
        let shell = self.shell();
        let script = RunScript {
            command,
            cwd,
            use_snapshot: shell.kind.is_posix(),
            persist: false,
            track_cwd: false,
            hidden: env.secret_names(),
        };
        let argv = run_argv(&shell, &self.state_files(), &script);
        let spawned = match self
            .setup
            .target
            .spawn(&env, &argv, &self.spawn_dir(cwd), true)
        {
            Ok(spawned) => spawned,
            Err(SpawnError::Refused(err)) => return TerminalResult::refused(err.message()),
            Err(SpawnError::Infrastructure(reason)) => return self.degraded(reason).await,
        };
        let id = self.processes.track(spawned, command, notify);
        self.started(id, started)
    }

    /// The result of a call that started background process `id`.
    fn started(&self, id: String, started: Started) -> TerminalResult {
        let mut result = TerminalResult::new(Status::Started);
        result.exit_code = None;
        result.hint = Some(format!(
            "running in the background; check it with process(action=\"poll\", process_id=\"{id}\")"
        ));
        if started.promoted {
            result.promoted_from_foreground = true;
            result.notices.push(format!(
                "promoted_from_foreground: the timeout exceeds terminal.max_foreground_secs ({}s), so the command runs as background process {id}; you will be notified when it finishes",
                self.setup.terminal.max_foreground_secs
            ));
        }
        if started.pty {
            result
                .notices
                .push("pty is not supported yet; the process runs with pipes".to_owned());
        }
        result.process_id = Some(id);
        result
    }

    /// An infrastructure failure: the environment is evicted (the next call
    /// recreates it) and the result is `degraded`, or `error` under
    /// `terminal.degraded_mode = "fail"`.
    async fn degraded(&self, reason: String) -> TerminalResult {
        {
            let mut state = self.env_state.lock().await;
            state.ready = false;
            state.evicted = true;
        }
        self.blocking(|target| target.evict()).await;
        tracing::warn!("terminal environment failed: {reason}");
        let retry_hint =
            "the environment will be recreated on the next call; retry the command".to_owned();
        let mut result = match self.setup.terminal.degraded_mode {
            DegradedMode::Degrade => TerminalResult::new(Status::Degraded),
            DegradedMode::Fail => TerminalResult::refused(reason.clone()),
        };
        result.reason = Some(reason);
        result.retry_hint = Some(retry_hint);
        result
    }
}

fn capture_into(
    capture: &Arc<Mutex<Capture>>,
    stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    let capture = Arc::clone(capture);
    tokio::spawn(async move {
        spawn::pump(stream, |bytes| {
            capture
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(bytes);
        })
        .await;
    })
}

/// The directory a shell reported. Git Bash's `pwd -W` writes `D:/a/..`;
/// Windows reports use the native separator.
fn observed_path(text: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(text.replace('/', "\\"))
    } else {
        PathBuf::from(text)
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || std::fs::canonicalize(a)
            .ok()
            .zip(std::fs::canonicalize(b).ok())
            .is_some_and(|(a, b)| a == b)
}

/// In-flight calls and the time the last one finished.
#[derive(Debug)]
struct Activity {
    in_flight: std::sync::atomic::AtomicUsize,
    last: Mutex<Instant>,
}

impl Default for Activity {
    fn default() -> Self {
        Self {
            in_flight: std::sync::atomic::AtomicUsize::new(0),
            last: Mutex::new(Instant::now()),
        }
    }
}

impl Activity {
    fn enter(self: &Arc<Self>) -> InFlight {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        InFlight(Arc::clone(self))
    }
}

/// What a session reports to a container's idle reaper (spec:
/// execution-environments, Container lifecycle and orphan reaping): a
/// foreground call in flight or a running background process keeps the
/// environment alive.
impl pmt_sandbox::container::Activity for Terminal {
    fn busy(&self) -> bool {
        self.activity.in_flight.load(Ordering::SeqCst) > 0 || !self.processes.running().is_empty()
    }

    fn idle_for(&self) -> Duration {
        self.activity
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .elapsed()
    }
}

struct InFlight(Arc<Activity>);

impl Drop for InFlight {
    fn drop(&mut self) {
        *self.0.last.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}
