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
use pmt_core::secret::Redactor;
use pmt_sandbox::Sandbox;
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

/// How long output readers may lag behind a foreground exit.
const READ_GRACE: Duration = Duration::from_millis(500);

/// Everything a session's local backend is built from.
pub struct LocalSetup {
    /// The session's `[terminal]` settings.
    pub terminal: TerminalConfig,
    /// `tool_output.max_chars`.
    pub max_chars: usize,
    /// `tools.kill_grace_secs`.
    pub kill_grace: Duration,
    /// The session sandbox (built from its effective capability set).
    pub sandbox: Arc<Sandbox>,
    /// The child environment (see [`ChildEnv::build`]).
    pub env: ChildEnv,
    /// The shell commands run in.
    pub shell: Shell,
    /// The session scratch directory (granted to the sandbox).
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

impl std::fmt::Debug for LocalSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalSetup")
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

/// What [`LocalTerminal::prepare`] hands to a run.
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

/// One session on the local backend.
pub struct LocalTerminal {
    setup: LocalSetup,
    files: StateFiles,
    chain: GuardChain,
    cwd: Mutex<PathBuf>,
    env_state: tokio::sync::Mutex<EnvState>,
    pending: Mutex<Pending>,
    processes: ProcessRegistry,
    runs: AtomicU64,
}

/// Things the next result carries once.
#[derive(Debug, Default)]
struct Pending {
    notices: Vec<String>,
    hint: Option<String>,
}

impl std::fmt::Debug for LocalTerminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalTerminal")
            .field("setup", &self.setup)
            .field("processes", &self.processes)
            .finish_non_exhaustive()
    }
}

impl LocalTerminal {
    /// A session backend. Creates the scratch directory and prunes spill
    /// files older than seven days.
    ///
    /// # Errors
    ///
    /// The scratch directory cannot be created.
    pub fn new(setup: LocalSetup) -> std::io::Result<Self> {
        pmt_core::private_fs::create_dir(&setup.scratch)?;
        if let Some(root) = setup.spill_dir.parent() {
            spill::prune(root, std::time::SystemTime::now());
        }
        let sandbox = Arc::clone(&setup.sandbox);
        let capability = FnGuard::new("capability", move |_: &GuardRequest| {
            sandbox
                .check()
                .map_err(|err| Stop::Denied(err.message().to_owned()))
        });
        let mut guards: Vec<Arc<dyn Guard>> = vec![Arc::new(capability)];
        guards.extend(setup.guards.guards().iter().cloned());
        guards.push(Arc::new(WorkdirGuard));
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
            max_chars: setup.max_chars,
            redactor: setup.redactor.clone(),
            notify: setup.notify.clone(),
            clock: setup
                .clock
                .clone()
                .unwrap_or_else(|| Arc::new(Instant::now)),
        });
        Ok(Self {
            files: StateFiles::in_dir(&setup.scratch),
            cwd: Mutex::new(setup.start_cwd.clone()),
            chain,
            env_state: tokio::sync::Mutex::default(),
            pending: Mutex::new(Pending {
                notices,
                hint: None,
            }),
            processes,
            runs: AtomicU64::new(0),
            setup,
        })
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

    /// Session end: stops every background process group.
    pub async fn close(&self) {
        self.processes.close().await;
    }

    /// Runs one `terminal` call.
    pub async fn run(&self, args: TerminalArgs) -> TerminalResult {
        let mut result = self.dispatch(args).await;
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
        let prepared = match self.prepare().await {
            Ok(prepared) => prepared,
            Err(SpawnError::Refused(err)) => return TerminalResult::refused(err.message()),
            Err(SpawnError::Infrastructure(reason)) => return self.degraded(reason).await,
        };
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
                self.background(&command, &cwd, notify, promoted, args.pty)
                    .await
            }
        };
        result.environment_recreated |= prepared.recreated;
        result
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
            Some(dir) => session.join(expand_home(dir, self.setup.user_home.as_deref())),
            None => session,
        }
    }

    /// Builds the shell environment once (and again after an eviction).
    async fn prepare(&self) -> Result<Prepared, SpawnError> {
        let mut state = self.env_state.lock().await;
        if state.ready {
            return Ok(Prepared { recreated: false });
        }
        if self.setup.shell.kind.is_posix() {
            self.init_shell().await?;
        }
        state.ready = true;
        Ok(Prepared {
            recreated: state.evicted,
        })
    }

    fn init_files(&self) -> Vec<PathBuf> {
        let home = self.setup.user_home.as_deref();
        let mut files = Vec::new();
        if self.setup.shell.kind == ShellKind::Bash
            && self.setup.terminal.auto_source_bashrc
            && let Some(rc) = home.map(|h| h.join(".bashrc")).filter(|rc| rc.is_file())
        {
            files.push(rc);
        }
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        for configured in &self.setup.terminal.shell_init_files {
            let path = expand_home(configured, home);
            if path.is_file() {
                files.push(path);
            } else {
                pending.notices.push(format!(
                    "terminal.shell_init_files: '{configured}' not found; skipped"
                ));
            }
        }
        files
    }

    /// Sources the init files into the snapshot under
    /// `terminal.shell_init_timeout_secs`; on timeout the shell runs without
    /// them and the next result carries [`INIT_TIMEOUT_HINT`].
    async fn init_shell(&self) -> Result<(), SpawnError> {
        let _ = std::fs::remove_file(&self.files.snapshot);
        let files = self.init_files();
        if files.is_empty() {
            return Ok(());
        }
        let argv = init_argv(
            &self.setup.shell,
            &self.files,
            self.setup.env.secret_names(),
            &files,
        );
        let spawned = spawn::spawn(
            &self.setup.sandbox,
            &self.setup.env,
            &argv,
            &self.setup.scratch,
            false,
        )?;
        let (mut child, group) = (spawned.child, spawned.group);
        let mut guard = GroupGuard::new(group);
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
            spawn::stop(&mut child, group, self.setup.kill_grace).await;
            let _ = std::fs::remove_file(&self.files.snapshot);
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
        let files = StateFiles {
            snapshot: self.files.snapshot.clone(),
            cwd: self.setup.scratch.join(format!("pmt-shell-cwd-{run}")),
        };
        let script = RunScript {
            command,
            cwd,
            use_snapshot: self.setup.shell.kind.is_posix(),
            persist: self.setup.terminal.persistent_shell,
            track_cwd: true,
            hidden: self.setup.env.secret_names(),
        };
        let argv = run_argv(&self.setup.shell, &files, &script);
        let spawned = match spawn::spawn(&self.setup.sandbox, &self.setup.env, &argv, cwd, false) {
            Ok(spawned) => spawned,
            Err(SpawnError::Refused(err)) => return TerminalResult::refused(err.message()),
            Err(SpawnError::Infrastructure(reason)) => return self.degraded(reason).await,
        };
        let (status, capture) = self.wait_foreground(spawned, timeout_secs).await;
        let shaped = capture.finish(
            self.setup.max_chars,
            &self.setup.hooks,
            &self.setup.redactor,
        );
        let mut result = TerminalResult::new(Status::Ok);
        result.output = shaped.output;
        result.truncated = shaped.truncated;
        result.full_output_path = shaped
            .full_output_path
            .map(|p| p.to_string_lossy().into_owned());
        if let Some(code) = status {
            result.exit_code = Some(code);
            result.status = if code == 0 { Status::Ok } else { Status::Error };
            result.exit_code_meaning = shape::exit_code_meaning(command, code);
            self.observe_cwd(&files.cwd, cwd, track, &mut result);
        } else {
            result.exit_code = Some(124);
            result.status = Status::Timeout;
            result.hint = Some(format!(
                "the command was stopped after {timeout_secs}s; pass a larger timeout, or background=true for long-running work"
            ));
        }
        let _ = std::fs::remove_file(&files.cwd);
        result
    }

    /// Waits for a foreground command (stopping its group at the timeout)
    /// while capturing its output. `None` means it timed out.
    async fn wait_foreground(
        &self,
        spawned: spawn::Spawned,
        timeout_secs: u64,
    ) -> (Option<i32>, Capture) {
        let (mut child, group) = (spawned.child, spawned.group);
        let mut guard = GroupGuard::new(group);
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
            spawn::stop(&mut child, group, self.setup.kill_grace).await;
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
    fn observe_cwd(&self, file: &Path, start: &Path, track: bool, result: &mut TerminalResult) {
        let Ok(text) = std::fs::read_to_string(file) else {
            return;
        };
        let observed = PathBuf::from(
            text.trim_end_matches(['\r', '\n'])
                .trim_start_matches('\u{feff}'),
        );
        if observed.as_os_str().is_empty() {
            return;
        }
        if !same_dir(&observed, start) {
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
        promoted: bool,
        pty: bool,
    ) -> TerminalResult {
        if let Err(message) = self.processes.reserve() {
            return TerminalResult::refused(message);
        }
        let script = RunScript {
            command,
            cwd,
            use_snapshot: self.setup.shell.kind.is_posix(),
            persist: false,
            track_cwd: false,
            hidden: self.setup.env.secret_names(),
        };
        let argv = run_argv(&self.setup.shell, &self.files, &script);
        let spawned = match spawn::spawn(&self.setup.sandbox, &self.setup.env, &argv, cwd, true) {
            Ok(spawned) => spawned,
            Err(SpawnError::Refused(err)) => return TerminalResult::refused(err.message()),
            Err(SpawnError::Infrastructure(reason)) => return self.degraded(reason).await,
        };
        let id = self.processes.track(spawned, command, notify);
        let mut result = TerminalResult::new(Status::Started);
        result.exit_code = None;
        result.hint = Some(format!(
            "running in the background; check it with process(action=\"poll\", process_id=\"{id}\")"
        ));
        if promoted {
            result.promoted_from_foreground = true;
            result.notices.push(format!(
                "promoted_from_foreground: the timeout exceeds terminal.max_foreground_secs ({}s), so the command runs as background process {id}; you will be notified when it finishes",
                self.setup.terminal.max_foreground_secs
            ));
        }
        if pty {
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

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || std::fs::canonicalize(a)
            .ok()
            .zip(std::fs::canonicalize(b).ok())
            .is_some_and(|(a, b)| a == b)
}
