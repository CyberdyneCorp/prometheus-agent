//! Inline shell snippets in skill bodies (spec: skills, Load-time template
//! substitution and inline shell).
//!
//! `` !`cmd` `` in a body is replaced, at load, by the command's output
//! when `skills.inline_shell = true`. Each snippet runs like a terminal
//! command of the session: through the approval gate, confined by the
//! session sandbox, with the session's child environment, in the skill
//! directory, under a deadline, its output capped. Failures render as
//! `[inline-shell error: <reason>]` instead of failing the load.

use std::ffi::OsString;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use pmt_sandbox::Sandbox;
use pmt_tools::approval::CommandGate;
use pmt_tools::dispatch::GateVerdict;
use pmt_tools::terminal::env::ChildEnv;
use pmt_tools::terminal::shell::Shell;
use pmt_tools::terminal::spawn::{self, SpawnError};
use regex::Regex;

/// Most characters of one snippet's output.
pub const MAX_OUTPUT_CHARS: usize = 4000;

/// Grace between SIGTERM and SIGKILL of a snippet past its deadline.
const KILL_GRACE: Duration = Duration::from_secs(1);

static SNIPPET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!`([^`\n]+)`").expect("snippet pattern"));

/// A boxed future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Runs one snippet; `Err` is the reason shown in the error marker.
pub trait InlineShell: Send + Sync {
    /// Runs `command` in `cwd` within `timeout`; returns its output.
    fn run<'a>(
        &'a self,
        command: &'a str,
        cwd: &'a Path,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<String, String>>;
}

/// The marker of a failed snippet.
#[must_use]
pub fn error_marker(reason: &str) -> String {
    format!("[inline-shell error: {reason}]")
}

/// Whether `body` holds any snippet.
#[must_use]
pub fn has_snippets(body: &str) -> bool {
    SNIPPET.is_match(body)
}

/// `body` with every snippet replaced by its output (or error marker).
pub async fn expand(
    body: &str,
    shell: Option<&dyn InlineShell>,
    cwd: &Path,
    timeout: Duration,
) -> String {
    let mut out = String::with_capacity(body.len());
    let mut last = 0;
    for captures in SNIPPET.captures_iter(body) {
        let (Some(all), Some(command)) = (captures.get(0), captures.get(1)) else {
            continue;
        };
        out.push_str(&body[last..all.start()]);
        let rendered = match shell {
            None => error_marker("no execution sandbox in this session"),
            Some(shell) => match shell.run(command.as_str(), cwd, timeout).await {
                Ok(output) => cap(output.trim_end()),
                Err(reason) => error_marker(&reason),
            },
        };
        out.push_str(&rendered);
        last = all.end();
    }
    out.push_str(&body[last..]);
    out
}

fn cap(text: &str) -> String {
    if text.chars().count() <= MAX_OUTPUT_CHARS {
        return text.to_owned();
    }
    let mut capped: String = text.chars().take(MAX_OUTPUT_CHARS).collect();
    capped.push_str("\n[output truncated]");
    capped
}

/// The production runner: the session's sandbox, child environment, shell
/// and approval gate.
pub struct SandboxShell {
    sandbox: Arc<Sandbox>,
    env: ChildEnv,
    shell: Shell,
    gate: Option<Arc<CommandGate>>,
}

impl SandboxShell {
    /// A runner; `gate` is the session's command gate (`None` only where
    /// the session has none, as in tests).
    #[must_use]
    pub fn new(
        sandbox: Arc<Sandbox>,
        env: ChildEnv,
        shell: Shell,
        gate: Option<Arc<CommandGate>>,
    ) -> Self {
        Self {
            sandbox,
            env,
            shell,
            gate,
        }
    }

    fn argv(&self, command: &str) -> Vec<OsString> {
        let mut argv = vec![self.shell.path.clone().into_os_string()];
        if self.shell.kind.is_posix() {
            argv.push("-c".into());
        } else {
            argv.extend([
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
            ]);
        }
        argv.push(command.into());
        argv
    }

    async fn approve(&self, command: &str) -> Result<(), String> {
        let Some(gate) = &self.gate else {
            return Ok(());
        };
        match gate
            .check_command("skill_view", "inline-shell", command)
            .await
        {
            GateVerdict::Allow => Ok(()),
            GateVerdict::Blocked(why) | GateVerdict::Denied(why) => Err(why),
        }
    }

    async fn execute(
        &self,
        command: &str,
        cwd: &Path,
        timeout: Duration,
    ) -> Result<String, String> {
        self.approve(command).await?;
        let spawned = spawn::spawn(&self.sandbox, &self.env, &self.argv(command), cwd, false)
            .map_err(|e| match e {
                SpawnError::Refused(err) => err.message().to_owned(),
                SpawnError::Infrastructure(why) => why,
            })?;
        let mut child = spawned.child;
        let output = Arc::new(Mutex::new(Vec::new()));
        let pumps = [
            child.stdout.take().map(boxed),
            child.stderr.take().map(boxed),
        ]
        .into_iter()
        .flatten()
        .map(|reader| {
            let sink = Arc::clone(&output);
            tokio::spawn(spawn::pump(reader, move |bytes| {
                sink.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .extend_from_slice(bytes);
            }))
        })
        .collect::<Vec<_>>();
        let status = if let Ok(status) = tokio::time::timeout(timeout, child.wait()).await {
            status.map_err(|e| e.to_string())?
        } else {
            spawn::stop(&mut child, spawned.group, KILL_GRACE).await;
            return Err(format!("timed out after {} ms", timeout.as_millis()));
        };
        for pump in pumps {
            let _ = pump.await;
        }
        let text = String::from_utf8_lossy(&output.lock().unwrap_or_else(PoisonError::into_inner))
            .into_owned();
        if status.success() {
            Ok(text)
        } else {
            let first = text.lines().next().unwrap_or_default();
            Err(format!("exit code {}: {first}", spawn::exit_code(status)))
        }
    }
}

fn boxed<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    reader: R,
) -> Pin<Box<dyn tokio::io::AsyncRead + Send>> {
    Box::pin(reader)
}

impl InlineShell for SandboxShell {
    fn run<'a>(
        &'a self,
        command: &'a str,
        cwd: &'a Path,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(self.execute(command, cwd, timeout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    impl InlineShell for Echo {
        fn run<'a>(
            &'a self,
            command: &'a str,
            _: &'a Path,
            _: Duration,
        ) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async move {
                if command == "fail" {
                    Err("exit code 1: nope".to_owned())
                } else {
                    Ok(format!("<{command}>\n"))
                }
            })
        }
    }

    #[tokio::test]
    async fn snippets_are_replaced_in_order() {
        let body = "a !`x` b !`fail` c `plain`";
        let out = expand(body, Some(&Echo), Path::new("."), Duration::from_secs(1)).await;
        assert_eq!(
            out,
            "a <x> b [inline-shell error: exit code 1: nope] c `plain`"
        );
        let none = expand("!`x`", None, Path::new("."), Duration::from_secs(1)).await;
        assert_eq!(
            none,
            "[inline-shell error: no execution sandbox in this session]"
        );
    }

    #[test]
    fn output_is_capped() {
        let long = "x".repeat(MAX_OUTPUT_CHARS + 10);
        assert!(cap(&long).starts_with(&"x".repeat(MAX_OUTPUT_CHARS)));
        assert!(cap(&long).ends_with("[output truncated]"));
    }
}
