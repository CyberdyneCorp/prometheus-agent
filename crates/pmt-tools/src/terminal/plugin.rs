//! Foreground commands on a `terminal-backend` plugin (spec:
//! execution-environments, Remote sandboxes are plugins).
//!
//! The command, its directory, its environment and the timeout go to the
//! plugin's `execute`; the reply's typed fields become the result. A reply
//! without `exit_code` is a contract violation reported as an error, never
//! read as success; a transport failure is a degraded result.

use std::sync::Arc;
use std::time::Duration;

use pmt_core::secret::Redactor;
use pmt_sandbox::backend::{ExecOutput, ExecRequest, RemoteBackend};

use super::env::ChildEnv;
use super::result::{Status, TerminalResult};
use super::shape::{self, OutputHook};
use super::spill::Capture;

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

/// Runs `command` on `plugin`.
pub(super) async fn run(
    plugin: &Arc<dyn RemoteBackend>,
    command: &str,
    cwd: &str,
    env: &ChildEnv,
    timeout_secs: u64,
    shaping: Shaping<'_>,
) -> Outcome {
    let request = ExecRequest {
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
        timeout_ms: timeout_secs.saturating_mul(1000),
    };
    let call = {
        let plugin = Arc::clone(plugin);
        let request = request.clone();
        tokio::task::spawn_blocking(move || plugin.execute(&request))
    };
    // The plugin bounds the command itself; a plugin that does not return
    // is stopped through `cancel` after a grace period.
    let limit = Duration::from_secs(timeout_secs.saturating_add(10));
    let output: ExecOutput = match tokio::time::timeout(limit, call).await {
        Ok(Ok(Ok(output))) => output,
        Ok(Ok(Err(why))) => {
            return Outcome::Failed(format!("terminal backend '{}': {why}", plugin.name()));
        }
        Ok(Err(join)) => {
            return Outcome::Failed(format!(
                "terminal backend '{}' failed: {join}",
                plugin.name()
            ));
        }
        Err(_) => {
            plugin.cancel(&request.id);
            let mut result = TerminalResult::new(Status::Timeout);
            result.exit_code = Some(124);
            result.hint = Some(format!("the command was stopped after {timeout_secs}s"));
            return Outcome::Ran(Box::new(result), None);
        }
    };
    let Some(code) = output.exit_code else {
        return Outcome::Ran(
            Box::new(TerminalResult::refused(format!(
                "terminal backend '{}' broke its contract: the execute result has no exit_code",
                plugin.name()
            ))),
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
