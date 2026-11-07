//! `prometheus verify` on the profile's terminal backend when it is not
//! the local one (spec: execution-environments, Project run-recipe
//! verification: phases run "on the session's backend"): the container
//! (the project mounted as its workspace), the SSH host (its configured
//! workspace) or a plugin backend runs each phase through the same
//! terminal sessions use, as a tracked background process waited for up
//! to the phase timeout. The `start` phase's port is probed inside the
//! backend (`nc -z`, else bash's `/dev/tcp`), where the server listens.

use std::sync::Arc;
use std::time::{Duration, Instant};

use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_sandbox::backend::BackendRegistry;
use pmt_tools::terminal::process::ProcessArgs;
use pmt_tools::terminal::{Terminal, TerminalArgs, TerminalSetup};
use serde_json::Value;

use super::{Outcome, Phase, Phases, Runner, VerifyArgs, millis, outcome_of, tail};

/// Characters of a phase's output the backend keeps for the tail.
const OUTPUT_CHARS: usize = 200_000;
/// How often a starting server's port is probed.
const PROBE_EVERY: Duration = Duration::from_millis(500);

/// Phases run on a non-local terminal backend.
pub(super) struct BackendRunner {
    runtime: tokio::runtime::Runtime,
    terminal: Arc<Terminal>,
    timeout: Duration,
    ready_timeout: Duration,
}

/// The runner of the profile's terminal backend, `None` when it is the
/// local one (phases then run under `host`'s sandbox).
///
/// # Errors
///
/// The selected backend cannot be set up (it never falls back to this
/// host), the async runtime cannot start.
pub(super) fn open(
    scope: &ProfileScope,
    launch: &LaunchEnv,
    host: &Runner,
    args: &VerifyArgs,
) -> Result<Option<BackendRunner>, Error> {
    let config = scope.config();
    let builtin = BackendRegistry::builtin();
    let registry = if builtin.select(&config.terminal.backend).is_ok() {
        builtin
    } else {
        let plugins = crate::plugins_host::for_session(scope, &host.dir);
        crate::chat::session::backend::registry(&plugins, &pmt_plugins::host::CallInfo::default())
    };
    // The session that ran `verify` shares its backend (a per-session
    // container is its own); a run from a shell gets a backend of its own.
    let session = launch
        .inherited("PROMETHEUS_SESSION_ID")
        .and_then(|v| v.to_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| {
            format!(
                "verify-{}",
                ulid::Ulid::generate().to_string().to_lowercase()
            )
        });
    let user_home = launch.user_home().cloned();
    let Some(remote) = crate::chat::session::backend::select(
        scope,
        &registry,
        &session,
        &host.dir,
        user_home.as_deref(),
    ) else {
        return Ok(None);
    };
    remote.target.check().map_err(|why| {
        Error::new(
            &codes::TERMINAL_BACKEND_UNAVAILABLE,
            format!("terminal backend '{}': {why}", config.terminal.backend),
        )
    })?;
    let terminal = Terminal::new(TerminalSetup {
        terminal: config.terminal.clone(),
        max_chars: pmt_tools::setting::fixed(OUTPUT_CHARS),
        kill_grace: Duration::from_secs(2),
        target: Arc::clone(&remote.target),
        env: host.env.clone(),
        shell: host.shell.clone(),
        scratch: host.scratch.clone(),
        spill_dir: host.scratch.join("spill"),
        start_cwd: remote.start_cwd.clone(),
        user_home,
        redactor: pmt_core::secret::Redactor::default(),
        hooks: Vec::new(),
        guards: pmt_tools::terminal::guard::GuardChain::default(),
        notices: Vec::new(),
        notify: None,
        auto_notify: false,
        receipts: None,
        before_command: None,
        clock: None,
    })
    .map_err(|e| Error::io(&codes::HOME_IO, &host.scratch, &e))?;
    let terminal = Arc::new(terminal);
    // The container's idle reaper must see the phases running, or it
    // tears the container down under them.
    if let Some(container) = &remote.container {
        let activity: Arc<dyn pmt_sandbox::container::Activity> = Arc::clone(&terminal) as _;
        container.attach(Arc::downgrade(&activity));
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| Error::io(&codes::HOME_IO, &host.scratch, &e))?;
    Ok(Some(BackendRunner {
        runtime,
        terminal,
        timeout: Duration::from_secs(args.timeout),
        ready_timeout: Duration::from_secs(args.ready_timeout),
    }))
}

impl Phases for BackendRunner {
    fn phase(&self, phase: &Phase, port: Option<u16>) -> Outcome {
        let started = Instant::now();
        let mut outcome = outcome_of(phase);
        self.runtime.block_on(async {
            match self.start(&phase.command).await {
                Err(why) => outcome.reason = Some(why),
                Ok(id) if phase.name == "start" => self.serve(&id, port, &mut outcome).await,
                Ok(id) => self.finish(&id, &mut outcome).await,
            }
        });
        outcome.duration_ms = millis(started.elapsed());
        outcome
    }
}

impl BackendRunner {
    /// Starts `command` as a background process; its id.
    async fn start(&self, command: &str) -> Result<String, String> {
        let started = self
            .terminal
            .run(TerminalArgs {
                background: true,
                ..TerminalArgs::new(command)
            })
            .await;
        match started.process_id {
            Some(id) => Ok(id),
            None => Err(started
                .error
                .or(started.reason)
                .unwrap_or_else(|| format!("cannot start: {}", started.output))),
        }
    }

    async fn process(&self, action: &str, id: &str, timeout: Option<Duration>) -> Value {
        self.terminal
            .processes()
            .handle(&ProcessArgs {
                action: action.to_owned(),
                process_id: Some(id.to_owned()),
                data: None,
                timeout: timeout.map(|t| t.as_secs_f64()),
            })
            .await
    }

    /// The process's output so far, as the tail of `outcome`.
    async fn collect(&self, id: &str, outcome: &mut Outcome) {
        let log = self.process("log", id, None).await;
        outcome.output_tail = tail(log["output"].as_str().unwrap_or_default());
    }

    /// Waits for a phase up to the timeout (then stops it).
    async fn finish(&self, id: &str, outcome: &mut Outcome) {
        let waited = self.process("wait", id, Some(self.timeout)).await;
        if waited["state"] == "running" {
            self.process("kill", id, None).await;
            outcome.reason = Some(format!("timed out after {} s", self.timeout.as_secs()));
        } else {
            outcome.exit_code = exit_code(&waited);
            outcome.passed = waited["state"] == "exited" && outcome.exit_code == Some(0);
        }
        self.collect(id, outcome).await;
    }

    /// Waits until the server listens on `port` inside the backend (or,
    /// without a port, keeps running for the ready timeout), then stops it.
    async fn serve(&self, id: &str, port: Option<u16>, outcome: &mut Outcome) {
        let deadline = Instant::now() + self.ready_timeout;
        loop {
            let polled = self.process("poll", id, None).await;
            if polled["state"] != "running" {
                outcome.exit_code = exit_code(&polled);
                outcome.reason = Some(format!(
                    "exited before it was ready (exit {})",
                    outcome
                        .exit_code
                        .map_or_else(|| "?".to_owned(), |c| c.to_string())
                ));
                break;
            }
            let listening = match port {
                Some(port) => self.listening(port).await,
                None => Instant::now() >= deadline,
            };
            if listening {
                outcome.passed = true;
                break;
            }
            if Instant::now() >= deadline {
                outcome.reason = Some(format!(
                    "not listening after {} s",
                    self.ready_timeout.as_secs()
                ));
                break;
            }
            tokio::time::sleep(PROBE_EVERY).await;
        }
        self.process("kill", id, None).await;
        self.collect(id, outcome).await;
    }

    /// Whether something accepts connections on `port` inside the backend.
    async fn listening(&self, port: u16) -> bool {
        let probe = format!(
            "nc -z 127.0.0.1 {port} 2>/dev/null || bash -c 'exec 3<>/dev/tcp/127.0.0.1/{port}' 2>/dev/null"
        );
        let result = self
            .terminal
            .run(TerminalArgs {
                timeout: Some(10.0),
                ..TerminalArgs::new(probe)
            })
            .await;
        result.exit_code == Some(0)
    }
}

fn exit_code(state: &Value) -> Option<i32> {
    state["exit_code"]
        .as_i64()
        .and_then(|c| i32::try_from(c).ok())
}

impl Drop for BackendRunner {
    /// Ends the backend session: a per-run container is removed, a
    /// persistent one is left for the sessions that share it.
    fn drop(&mut self) {
        self.runtime.block_on(self.terminal.close());
    }
}
