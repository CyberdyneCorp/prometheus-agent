//! Programmatic tool calling: `execute_code` (spec: subagent-delegation,
//! Programmatic tool calling under the caller's grant; `execute_code` RPC
//! channel and limits; `execute_code` pre-execution guards).
//!
//! A Python script runs in the calling session's OS sandbox with an empty
//! environment (only `PATH`, `HOME` set to a scratch directory, `LANG` and
//! the RPC variables), so no secret reaches it. It calls tools through
//! generated stubs (`from prometheus_tools import read_file`) that talk to
//! the host over a per-execution [`channel`] authenticated by a 256-bit
//! token; the host re-checks every call against the session's grant and
//! dispatches it through the session's own tool pipeline. Only the
//! intersection of `code_execution.allowed_tools` with the caller's
//! granted tools is exposed, and an empty intersection runs nothing.

pub mod channel;
pub mod interpreter;
pub mod kernel;
pub mod output;
pub mod stubs;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use pmt_core::cancel::CancelToken;
use pmt_core::error::{Error, codes};
use pmt_sandbox::Sandbox;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt as _;

use crate::ToolResult;

pub use channel::{StubCalls, Token, unauthorized};
pub use output::{STDERR_CAP, STDOUT_CAP, cap_stdout};

/// The message of an empty `code`.
pub const NO_CODE: &str = "No code provided.";
/// The message when the session has no sandbox to run the script in.
pub const NO_SANDBOX: &str = "execute_code refused: sandbox policy unavailable";

/// The stub tools of a session: `allowed` (the configuration) that the
/// session was granted, in configuration order.
///
/// # Errors
///
/// `execute_code.no_tools` when none is left: no other tool is ever
/// exposed instead.
pub fn stub_tools(allowed: &[String], granted: &[String]) -> Result<Vec<String>, Error> {
    let tools: Vec<String> = allowed
        .iter()
        .filter(|t| granted.contains(t))
        .cloned()
        .collect();
    if tools.is_empty() {
        return Err(Error::new(
            &codes::EXECUTE_CODE_NO_TOOLS,
            "execute_code refused: none of code_execution.allowed_tools is granted to this session, \
             so no tool could be exposed to the script.",
        ));
    }
    Ok(tools)
}

/// One script run.
pub struct Execution<'a> {
    /// The Python source.
    pub code: &'a str,
    /// The interpreter.
    pub python: &'a Path,
    /// The session's sandbox.
    pub sandbox: &'a Sandbox,
    /// The script's working directory.
    pub workdir: &'a Path,
    /// A directory the session may write (the execution's files go below).
    pub scratch: &'a Path,
    /// The stub tools.
    pub tools: Vec<String>,
    /// Stub calls allowed.
    pub max_tool_calls: u32,
    /// Wall-clock limit.
    pub timeout: Duration,
    /// Where the full stdout is kept when it is capped.
    pub spill_dir: &'a Path,
    /// Why the script runs differently than configured (a fallback),
    /// added to the result.
    pub note: Option<String>,
}

/// The refusal of an `execute_code` call.
#[must_use]
pub fn refusal(err: &Error) -> ToolResult {
    let mut result = ToolResult::error(err.message().to_owned());
    result
        .metadata
        .insert("code".to_owned(), err.code().code.into());
    result
}

/// Runs `exec`, answering its stub calls through `calls`; `cancel` (the
/// turn's hard stop) kills the script.
pub async fn run(exec: Execution<'_>, calls: &dyn StubCalls, cancel: &CancelToken) -> ToolResult {
    if exec.code.trim().is_empty() {
        return ToolResult::error(NO_CODE);
    }
    match run_script(&exec, calls, cancel).await {
        Ok(result) => result,
        Err(err) => refusal(&err),
    }
}

async fn run_script(
    exec: &Execution<'_>,
    calls: &dyn StubCalls,
    cancel: &CancelToken,
) -> Result<ToolResult, Error> {
    let dir = exec
        .scratch
        .join(format!("execute-code-{}", ulid::Ulid::generate()));
    let io = |e: std::io::Error| Error::io(&codes::HOME_IO, &dir, &e);
    let home = dir.join("home");
    std::fs::create_dir_all(&home).map_err(io)?;
    let token = Token::new();
    let rpc = dir.join("rpc");
    std::fs::create_dir_all(&rpc).map_err(io)?;
    std::fs::write(dir.join("prometheus_tools.py"), stubs::module(&exec.tools)).map_err(io)?;
    let script = dir.join("script.py");
    std::fs::write(&script, exec.code).map_err(io)?;
    let mut child = spawn(exec, &script, &home, &rpc, &token)?;
    let server = channel::Server::new(rpc, token, exec.tools.clone(), exec.max_tool_calls);
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let read_out = async {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out).await;
        out
    };
    let read_err = async {
        let mut err = Vec::new();
        let _ = stderr.read_to_end(&mut err).await;
        err
    };
    let waited = async {
        tokio::select! {
            status = child.wait() => Ended::Exited(status.ok().and_then(|s| s.code())),
            () = tokio::time::sleep(exec.timeout) => Ended::TimedOut,
            () = cancel.cancelled() => Ended::Cancelled,
        }
    };
    let serving = server.serve(calls);
    let ((ended, out, err), calls_made) = tokio::select! {
        done = async { tokio::join!(waited, read_out, read_err) } => (done, server.calls()),
        // The channel serves until the script ends; it never returns first.
        () = serving => unreachable!("the channel serves until the script ends"),
    };
    if !matches!(ended, Ended::Exited(_)) {
        let _ = child.kill().await;
    }
    let mut body = output::body(&output::Ran {
        stdout: &out,
        stderr: &err,
        exit_code: match ended {
            Ended::Exited(code) => code,
            _ => None,
        },
        timed_out: matches!(ended, Ended::TimedOut),
        cancelled: matches!(ended, Ended::Cancelled),
        tool_calls: calls_made,
        timeout: exec.timeout,
        spill_dir: exec.spill_dir,
    });
    if let Some(note) = &exec.note {
        body["note"] = note.clone().into();
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(output::finish(&body, ended_ok(&ended)))
}

enum Ended {
    Exited(Option<i32>),
    TimedOut,
    Cancelled,
}

fn ended_ok(ended: &Ended) -> bool {
    matches!(ended, Ended::Exited(Some(0)))
}

/// The script under the sandbox with nothing but the allowed environment.
fn spawn(
    exec: &Execution<'_>,
    script: &Path,
    home: &Path,
    rpc: &Path,
    token: &Token,
) -> Result<tokio::process::Child, Error> {
    let mut command = exec.sandbox.command(exec.python)?;
    command
        .args(["-E", "-s"])
        .arg(script)
        .current_dir(exec.workdir)
        .env_clear()
        .envs(environment(home, rpc, token))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    command.spawn().map_err(|e| {
        Error::new(
            &codes::EXECUTE_CODE_REFUSED,
            format!("execute_code could not start Python: {e}"),
        )
    })
}

/// The script's whole environment (the stub module next to it is found
/// on `sys.path`; `-E -s` keep Python from reading user or `PYTHON*`
/// settings).
fn environment(home: &Path, rpc: &Path, token: &Token) -> Vec<(String, String)> {
    let mut env = vec![
        ("PATH".to_owned(), default_path()),
        ("HOME".to_owned(), home.to_string_lossy().into_owned()),
        ("LANG".to_owned(), "C.UTF-8".to_owned()),
        (
            channel::DIR_VAR.to_owned(),
            rpc.to_string_lossy().into_owned(),
        ),
        (channel::TOKEN_VAR.to_owned(), token.as_str().to_owned()),
    ];
    if cfg!(windows) {
        // CPython cannot start on Windows without its system root.
        if let Ok(root) = std::env::var("SYSTEMROOT") {
            env.push(("SYSTEMROOT".to_owned(), root));
        }
    }
    env
}

fn default_path() -> String {
    if cfg!(windows) {
        r"C:\Windows\System32;C:\Windows".to_owned()
    } else {
        "/usr/local/bin:/usr/bin:/bin".to_owned()
    }
}

/// The JSON a stub sees for `result`.
#[must_use]
pub fn stub_reply(result: &ToolResult) -> Value {
    json!({"ok": result.status.succeeded(), "result": result.text_content()})
}
