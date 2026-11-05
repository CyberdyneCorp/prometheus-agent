//! Commands on a `terminal-backend` plugin (spec: execution-environments,
//! Remote sandboxes are plugins; Foreground cap, promotion and server
//! guard; Background process registry).
//!
//! The command, its directory, its environment and the timeout go to the
//! plugin's `execute`; the reply's typed fields become the result. A reply
//! without `exit_code` is a contract violation reported as an error, never
//! read as success; a transport failure is a degraded result.
//!
//! A background command is the same `execute` call made by a host task the
//! process registry tracks ([`job`]): its output is delivered when the
//! call returns, and `kill`, session end or the timeout ask the plugin to
//! `cancel` it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pmt_core::secret::Redactor;
use pmt_sandbox::backend::{ExecOutput, ExecRequest, RemoteBackend};

use super::env::ChildEnv;
use super::process::{Job, JobEnd};
use super::result::{Status, TerminalResult};
use super::shape::{self, OutputHook};
use super::spawn::{Group, RemoteGroup};
use super::spill::Capture;

/// How long a plugin may take past the command's own timeout.
const GRACE: Duration = Duration::from_secs(10);

/// `timeout_ms` of a background command without a timeout: no limit (it
/// runs until it ends, is killed or the session ends, like a local one).
const UNBOUNDED_MS: u64 = u64::MAX;

/// How a plugin command is shaped.
pub(super) struct Shaping<'a> {
    pub(super) spill_dir: &'a std::path::Path,
    pub(super) max_chars: usize,
    pub(super) hooks: &'a [OutputHook],
    pub(super) redactor: &'a Redactor,
}

/// What a plugin call produced.
pub(super) enum Outcome {
    /// It ran (or broke the contract): the result and the observed cwd.
    Ran(Box<TerminalResult>, Option<String>),
    /// The backend failed: degrade with this reason.
    Failed(String),
}

/// Why a plugin call produced no output.
enum Failure {
    /// The backend or its transport failed.
    Backend(String),
    /// The call outlived its limit and was cancelled.
    Timeout,
}

fn request(command: &str, cwd: &str, env: &ChildEnv, timeout_ms: u64) -> ExecRequest {
    ExecRequest {
        id: ulid::Ulid::generate().to_string(),
        command: command.to_owned(),
        cwd: cwd.to_owned(),
        env: env
            .names()
            .filter_map(|n| {
                env.get(n)
                    .map(|v| (n.to_owned(), v.to_string_lossy().into_owned()))
            })
            .collect(),
        timeout_ms,
    }
}

/// Calls `execute` off the async runtime. The plugin bounds the command
/// itself; a plugin that does not return within `limit` plus a grace
/// period is stopped through `cancel`.
async fn call(
    plugin: &Arc<dyn RemoteBackend>,
    request: &ExecRequest,
    limit: Option<Duration>,
) -> Result<ExecOutput, Failure> {
    let call = {
        let plugin = Arc::clone(plugin);
        let request = request.clone();
        tokio::task::spawn_blocking(move || plugin.execute(&request))
    };
    let joined = match limit {
        Some(limit) => {
            let Ok(joined) = tokio::time::timeout(limit.saturating_add(GRACE), call).await else {
                cancel(plugin, &request.id);
                return Err(Failure::Timeout);
            };
            joined
        }
        None => call.await,
    };
    match joined {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(why)) => Err(Failure::Backend(format!(
            "terminal backend '{}': {why}",
            plugin.name()
        ))),
        Err(join) => Err(Failure::Backend(format!(
            "terminal backend '{}' failed: {join}",
            plugin.name()
        ))),
    }
}

/// Asks the plugin to stop command `id` without blocking the caller (the
/// plugin's `cancel` may itself wait on its remote service).
fn cancel(plugin: &Arc<dyn RemoteBackend>, id: &str) {
    let plugin = Arc::clone(plugin);
    let id = id.to_owned();
    std::thread::spawn(move || plugin.cancel(&id));
}

fn contract_violation(plugin: &dyn RemoteBackend) -> String {
    format!(
        "terminal backend '{}' broke its contract: the execute result has no exit_code",
        plugin.name()
    )
}

/// Runs `command` on `plugin` in the foreground.
pub(super) async fn run(
    plugin: &Arc<dyn RemoteBackend>,
    command: &str,
    cwd: &str,
    env: &ChildEnv,
    timeout_secs: u64,
    shaping: Shaping<'_>,
) -> Outcome {
    let request = request(command, cwd, env, timeout_secs.saturating_mul(1000));
    let output = match call(plugin, &request, Some(Duration::from_secs(timeout_secs))).await {
        Ok(output) => output,
        Err(Failure::Backend(reason)) => return Outcome::Failed(reason),
        Err(Failure::Timeout) => {
            let mut result = TerminalResult::new(Status::Timeout);
            result.exit_code = Some(124);
            result.hint = Some(format!("the command was stopped after {timeout_secs}s"));
            return Outcome::Ran(Box::new(result), None);
        }
    };
    let Some(code) = output.exit_code else {
        return Outcome::Ran(
            Box::new(TerminalResult::refused(contract_violation(plugin.as_ref()))),
            None,
        );
    };
    let mut capture = Capture::new(shaping.spill_dir);
    capture.push(output.stdout.as_bytes());
    capture.push(output.stderr.as_bytes());
    let shaped = capture
        .finish(shaping.max_chars, shaping.hooks, shaping.redactor)
        .await;
    let mut result = TerminalResult::new(if code == 0 { Status::Ok } else { Status::Error });
    result.output = shaped.output;
    result.truncated = shaped.truncated;
    result.full_output_path = shaped
        .full_output_path
        .map(|p| p.to_string_lossy().into_owned());
    result.exit_code = Some(code);
    result.exit_code_meaning = shape::exit_code_meaning(command, code);
    Outcome::Ran(Box::new(result), output.cwd_observed)
}

/// Starts `command` on `plugin` as a background [`Job`]; `timeout_secs`
/// bounds it when the call gave a timeout.
pub(super) fn job(
    plugin: &Arc<dyn RemoteBackend>,
    command: &str,
    cwd: &str,
    env: &ChildEnv,
    timeout_secs: Option<u64>,
) -> Job {
    let timeout_ms = timeout_secs.map_or(UNBOUNDED_MS, |t| t.saturating_mul(1000));
    let request = request(command, cwd, env, timeout_ms);
    let stop = Arc::new(Cancel {
        plugin: Arc::clone(plugin),
        id: request.id.clone(),
        sent: AtomicBool::new(false),
    });
    let group = Group::local(None).with_remote(Arc::clone(&stop) as Arc<dyn RemoteGroup>);
    let plugin = Arc::clone(plugin);
    let run = async move {
        let limit = timeout_secs.map(Duration::from_secs);
        let ended = call(&plugin, &request, limit).await;
        // A finished call is never cancelled (session end signals every
        // group it tracked).
        stop.sent.store(true, Ordering::SeqCst);
        match ended {
            Ok(output) => match output.exit_code {
                Some(exit_code) => JobEnd {
                    exit_code,
                    output: output.stdout + &output.stderr,
                },
                None => JobEnd {
                    exit_code: -1,
                    output: format!(
                        "{}{}{}\n",
                        output.stdout,
                        output.stderr,
                        contract_violation(plugin.as_ref())
                    ),
                },
            },
            Err(Failure::Backend(reason)) => JobEnd {
                exit_code: -1,
                output: format!("{reason}\n"),
            },
            Err(Failure::Timeout) => JobEnd {
                exit_code: 124,
                output: format!(
                    "the command was stopped after {}s\n",
                    timeout_secs.unwrap_or_default()
                ),
            },
        }
    };
    Job {
        run: Box::pin(run),
        group,
    }
}

/// The remote side of a plugin job's [`Group`]: the first signal asks the
/// plugin to cancel the call.
#[derive(Debug)]
struct Cancel {
    plugin: Arc<dyn RemoteBackend>,
    id: String,
    sent: AtomicBool,
}

impl RemoteGroup for Cancel {
    fn signal(&self, _kill: bool) {
        if !self.sent.swap(true, Ordering::SeqCst) {
            cancel(&self.plugin, &self.id);
        }
    }
}
