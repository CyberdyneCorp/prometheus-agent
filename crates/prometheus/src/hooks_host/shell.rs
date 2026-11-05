//! Shell hooks (spec: plugin-system, Shell hooks with first-use consent):
//! `[[hooks.<event>]]` commands run as sandboxed subprocesses with the
//! event's JSON on stdin; their stdout, read as JSON, is the directive the
//! dispatcher receives.
//!
//! The protocol: stdin is `{hook_event_name, tool_name, tool_input,
//! session_id, cwd, profile, extra}`. Stdout may answer `{action|decision =
//! "block", message|reason}`, `{action = "modify", args}` (or `{decision =
//! "modify", tool_input}`), `{action = "approve", message?, rule_key?}`,
//! `{context}` (`pre_llm_call`) or `{action = "continue", message}`
//! (`pre_verify`); empty stdout is no answer. Exit code 2 from a
//! `pre_tool_call` hook blocks the call with its stdout, else the first
//! 400 characters of stderr, else `Blocked by shell hook.`. Any other
//! failure fails open with a warning, unless the hook is `fail_closed`
//! (honoured for `pre_tool_call` only), which blocks with `hook <command>
//! failed closed: <reason>`.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_config::schema::ShellHook;
use pmt_plugins::hooks::{ExternalHook, HookFuture, Payload};
use pmt_plugins::host::CallInfo;
use pmt_plugins::host::hook_world::{Approval, Reply};
use serde_json::{Map, Value, json};
use tokio::io::AsyncWriteExt as _;

/// The longest stderr text a blocking exit 2 carries.
const STDERR_REASON_CHARS: usize = 400;
/// What an exit 2 without any output says.
pub const BLOCKED: &str = "Blocked by shell hook.";
/// Hooks whose matcher is read (the tool name).
const TOOL_EVENTS: &[&str] = &["pre_tool_call", "post_tool_call", "transform_tool_result"];

/// How one run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ran {
    /// It exited with `code` (`None`: killed by a signal).
    Exited {
        /// The exit code.
        code: Option<i32>,
        /// Its stdout.
        stdout: String,
        /// Its stderr.
        stderr: String,
    },
    /// It could not start.
    NotStarted(String),
    /// It overran its timeout and was killed.
    TimedOut(Duration),
}

/// The directive a run amounts to: `Ok(None)` is no answer, `Err` a
/// failure that fails open (logged).
///
/// # Errors
///
/// Why the hook failed.
pub fn interpret(
    event: &str,
    command: &str,
    fail_closed: bool,
    ran: &Ran,
) -> Result<Option<Reply>, String> {
    let failure = match ran {
        Ran::Exited {
            code: Some(2),
            stdout,
            stderr,
        } if event == "pre_tool_call" => {
            return Ok(Some(Reply::Block(exit_two_reason(stdout, stderr))));
        }
        Ran::Exited {
            code: Some(0),
            stdout,
            ..
        } => match directive(event, stdout) {
            Ok(reply) => return Ok(reply),
            Err(why) => why,
        },
        Ran::Exited { code, stderr, .. } => {
            let code = code.map_or_else(|| "a signal".to_owned(), |c| c.to_string());
            let detail: String = stderr.trim().chars().take(STDERR_REASON_CHARS).collect();
            if detail.is_empty() {
                format!("exited with {code}")
            } else {
                format!("exited with {code}: {detail}")
            }
        }
        Ran::NotStarted(why) => format!("could not start: {why}"),
        Ran::TimedOut(after) => format!("timed out after {} s", after.as_secs()),
    };
    if fail_closed && event == "pre_tool_call" {
        Ok(Some(Reply::Block(format!(
            "hook {command} failed closed: {failure}"
        ))))
    } else {
        Err(failure)
    }
}

fn exit_two_reason(stdout: &str, stderr: &str) -> String {
    let stdout = stdout.trim();
    if !stdout.is_empty() {
        return serde_json::from_str::<Value>(stdout)
            .ok()
            .and_then(|v| text(&v, &["message", "reason"]))
            .unwrap_or_else(|| stdout.to_owned());
    }
    let stderr: String = stderr.trim().chars().take(STDERR_REASON_CHARS).collect();
    if stderr.is_empty() {
        BLOCKED.to_owned()
    } else {
        stderr
    }
}

fn text(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| value.get(k).and_then(Value::as_str))
        .map(str::to_owned)
}

/// The directive of a successful run's stdout.
fn directive(event: &str, stdout: &str) -> Result<Option<Reply>, String> {
    let stdout = stdout.trim();
    if stdout.is_empty() {
        return Ok(None);
    }
    let value: Value =
        serde_json::from_str(stdout).map_err(|e| format!("stdout is not JSON ({e})"))?;
    let verb = text(&value, &["action", "decision"]);
    Ok(match verb.as_deref() {
        Some("block") => Some(Reply::Block(
            text(&value, &["message", "reason"]).unwrap_or_else(|| BLOCKED.to_owned()),
        )),
        Some("modify") => value
            .get("args")
            .or_else(|| value.get("tool_input"))
            .filter(|v| v.is_object())
            .map(|v| Reply::Modify(v.to_string())),
        Some("approve") => Some(Reply::Approve(Approval {
            message: text(&value, &["message"]).unwrap_or_default(),
            rule_key: text(&value, &["rule_key"]),
        })),
        Some("continue") if event == "pre_verify" => text(&value, &["message"]).map(|message| {
            Reply::Modify(json!({"action": "continue", "message": message}).to_string())
        }),
        _ => text(&value, &["context"]).map(Reply::Context),
    })
}

/// The stdin document of one firing.
#[must_use]
pub fn stdin_document(
    event: &str,
    payload: &Map<String, Value>,
    call: &CallInfo,
    profile: &str,
) -> Value {
    let mut extra = payload.clone();
    let tool_name = extra.remove("tool").unwrap_or(Value::Null);
    let tool_input = extra.remove("arguments").unwrap_or(Value::Null);
    extra.remove("schema_version");
    let session = extra
        .remove("session_id")
        .or_else(|| call.session_id.clone().map(Value::from))
        .unwrap_or(Value::Null);
    json!({
        "hook_event_name": event,
        "tool_name": tool_name,
        "tool_input": tool_input,
        "session_id": session,
        "cwd": call.cwd,
        "profile": profile,
        "extra": extra,
    })
}

/// One approved `[[hooks.<event>]]` entry, ready to run.
pub struct ShellRunner {
    label: String,
    event: String,
    hook: ShellHook,
    matcher: Option<regex::Regex>,
    words: Vec<String>,
    program: PathBuf,
    scripts: Vec<PathBuf>,
    scope: ProfileScope,
}

impl ShellRunner {
    /// A runner of `entry` whose program resolved to `program`.
    #[must_use]
    pub fn new(scope: ProfileScope, entry: &super::consent::ShellEntry, program: PathBuf) -> Self {
        let hook = entry.hook.clone();
        let matcher = hook
            .matcher
            .as_deref()
            .and_then(|m| regex::Regex::new(m).ok());
        Self {
            label: format!("shell:{}", hook.command),
            event: entry.event.clone(),
            hook,
            matcher,
            words: entry.words.clone(),
            program,
            scripts: entry.scripts.clone(),
            scope,
        }
    }

    /// Runs the hook once on `stdin`.
    pub async fn run(&self, stdin: &Value, cwd: Option<&str>) -> Ran {
        let mut command = match super::sandboxed(&self.scope, &self.program, &self.scripts, cwd) {
            Ok(command) => tokio::process::Command::from(command),
            Err(err) => return Ran::NotStarted(err.message().to_owned()),
        };
        command
            .args(&self.words[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        run_child(command, stdin, self.hook.timeout()).await
    }
}

/// Spawns `command`, writes `stdin`, and collects its output within
/// `timeout`.
pub async fn run_child(
    mut command: tokio::process::Command,
    stdin: &Value,
    timeout: Duration,
) -> Ran {
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return Ran::NotStarted(err.to_string()),
    };
    if let Some(mut pipe) = child.stdin.take() {
        // A hook that never reads its stdin still runs.
        let _ = pipe.write_all(stdin.to_string().as_bytes()).await;
        drop(pipe);
    }
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => Ran::Exited {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Ok(Err(err)) => Ran::NotStarted(err.to_string()),
        Err(_) => Ran::TimedOut(timeout),
    }
}

impl ExternalHook for ShellRunner {
    fn label(&self) -> &str {
        &self.label
    }

    fn applies(&self, hook: &str, payload: &Payload, _call: &CallInfo) -> bool {
        let Some(matcher) = &self.matcher else {
            return true;
        };
        if !TOOL_EVENTS.contains(&hook) {
            return true;
        }
        payload
            .fields()
            .get("tool")
            .and_then(Value::as_str)
            .is_some_and(|tool| matcher.is_match(tool))
    }

    fn call<'a>(
        &'a self,
        hook: &'a str,
        payload: &'a Payload,
        call: &'a CallInfo,
    ) -> HookFuture<'a> {
        Box::pin(async move {
            let stdin = stdin_document(hook, payload.fields(), call, self.scope.name().as_str());
            let ran = self.run(&stdin, call.cwd.as_deref()).await;
            interpret(&self.event, &self.hook.command, self.hook.fail_closed, &ran)
        })
    }
}

/// Shares a runner as an external callback.
#[must_use]
pub fn external(runner: ShellRunner) -> Arc<dyn ExternalHook> {
    Arc::new(runner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exited(code: i32, stdout: &str, stderr: &str) -> Ran {
        Ran::Exited {
            code: Some(code),
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
        }
    }

    /// The directive in a comparable form.
    fn shown(reply: Option<Reply>) -> String {
        match reply {
            None | Some(Reply::None) => "none".to_owned(),
            Some(Reply::Block(m)) => format!("block {m}"),
            Some(Reply::Modify(m)) => format!("modify {m}"),
            Some(Reply::Context(m)) => format!("context {m}"),
            Some(Reply::Approve(a)) => format!("approve {}", a.message),
        }
    }

    #[test]
    fn exit_two_blocks_with_stdout_then_stderr_then_the_default() {
        let block = |ran: &Ran| shown(interpret("pre_tool_call", "x", false, ran).unwrap());
        assert_eq!(
            block(&exited(2, "", "policy violation\n")),
            "block policy violation"
        );
        assert_eq!(
            block(&exited(2, "use the API", "noise")),
            "block use the API"
        );
        assert_eq!(block(&exited(2, "", "")), format!("block {BLOCKED}"));
        // Exit 2 of another event is an ordinary failure.
        assert!(interpret("post_tool_call", "x", false, &exited(2, "", "")).is_err());
    }

    #[test]
    fn failures_fail_open_unless_fail_closed_on_pre_tool_call() {
        let missing = Ran::NotStarted("No such file or directory".into());
        assert!(interpret("pre_tool_call", "./nope", false, &missing).is_err());
        assert_eq!(
            shown(interpret("pre_tool_call", "./nope", true, &missing).unwrap()),
            "block hook ./nope failed closed: could not start: No such file or directory"
        );
        assert!(interpret("post_tool_call", "./nope", true, &missing).is_err());
    }

    #[test]
    fn stdout_directives_are_read_in_both_shapes() {
        let read = |event: &str, out: &str| {
            shown(interpret(event, "x", false, &exited(0, out, "")).unwrap())
        };
        assert_eq!(
            read("pre_tool_call", r#"{"decision":"block","reason":"no"}"#),
            "block no"
        );
        assert_eq!(
            read(
                "pre_tool_call",
                r#"{"decision":"modify","tool_input":{"a":1}}"#
            ),
            r#"modify {"a":1}"#
        );
        assert_eq!(
            read("pre_llm_call", r#"{"context":"be brief"}"#),
            "context be brief"
        );
        assert_eq!(read("pre_tool_call", ""), "none");
        assert!(interpret("pre_tool_call", "x", false, &exited(0, "not json", "")).is_err());
    }
}
