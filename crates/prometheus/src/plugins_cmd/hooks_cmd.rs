//! `prometheus plugins hooks list|test|allow|revoke|doctor` (spec:
//! plugin-system, Hooks CLI; Shell hooks with first-use consent; Gateway
//! hook directories; Outbound webhooks).

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

use clap::Subcommand;
use pmt_config::ProfileScope;
use pmt_config::grants::{GrantsFile, HookConsent, HookKind};
use pmt_core::error::{Error, codes};
use pmt_plugins::host::CallInfo;
use pmt_plugins::host::hook_world::Reply;
use serde_json::{Map, Value, json};

use crate::context::Context;
use crate::hooks_host::consent::{self, Consent, ShellEntry};
use crate::hooks_host::dirs;
use crate::hooks_host::shell::{Ran, ShellRunner, interpret, stdin_document};
use crate::output::print_raw;

/// `prometheus plugins hooks` actions.
#[derive(Debug, Subcommand)]
pub enum HooksAction {
    /// Shell hooks (matcher, timeout, consent), gateway hook directories
    /// and outbound webhooks (`signed` or `UNSIGNED`).
    List,
    /// Fires the shell hooks of `<event>` on a synthetic payload and
    /// prints each directive exactly as the dispatcher would receive it.
    Test {
        /// The hook event (`pre_tool_call`...).
        event: String,
        /// The tool name of a tool event (matchers apply to it).
        #[arg(long, value_name = "TOOL")]
        for_tool: Option<String>,
        /// A JSON object merged into the synthetic payload.
        #[arg(long, value_name = "PATH")]
        payload_file: Option<PathBuf>,
    },
    /// Approves a shell hook (`<event> <command>`, the command as written
    /// in config.toml) or, with `--dir`, a gateway hook directory, at its
    /// current program or handler digest.
    Allow {
        /// The event.
        #[arg(required_unless_present = "dir")]
        event: Option<String>,
        /// The command.
        #[arg(required_unless_present = "dir")]
        command: Option<String>,
        /// A hook directory under `<profile_home>/hooks/`.
        #[arg(long, value_name = "NAME", conflicts_with_all = ["event", "command"])]
        dir: Option<String>,
    },
    /// Removes the approvals of a shell hook command (every event) or of a
    /// hook directory name.
    Revoke {
        /// The command, or the directory name.
        command: String,
    },
    /// Checks every hook: program present and executable, consent, digest
    /// drift, JSON on stdout and execution time; exit 1 on any failure.
    Doctor,
}

/// Runs a `plugins hooks` action.
///
/// # Errors
///
/// Profile, grants and output errors.
pub fn run(ctx: &Context<'_>, action: &HooksAction) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    match action {
        HooksAction::List => list(&scope),
        HooksAction::Test {
            event,
            for_tool,
            payload_file,
        } => crate::model_cmd::block_on(test(
            &scope,
            event,
            for_tool.as_deref(),
            payload_file.as_deref(),
        ))?,
        HooksAction::Allow {
            event,
            command,
            dir,
        } => allow(&scope, event.as_deref(), command.as_deref(), dir.as_deref()),
        HooksAction::Revoke { command } => revoke(&scope, command),
        HooksAction::Doctor => crate::model_cmd::block_on(doctor(&scope))?,
    }
}

fn list(scope: &ProfileScope) -> Result<u8, Error> {
    let grants = GrantsFile::of(scope.profile()).load()?;
    let mut text = String::from("shell hooks:\n");
    let entries = consent::shell_entries(scope);
    if entries.is_empty() {
        text.push_str("  (none)\n");
    }
    for entry in &entries {
        let hook = &entry.hook;
        let _ = writeln!(
            text,
            "  {}  {}  matcher={}  timeout={}s{}  {}",
            entry.event,
            hook.command,
            hook.matcher.as_deref().unwrap_or("*"),
            hook.timeout().as_secs(),
            if hook.fail_closed {
                "  fail_closed"
            } else {
                ""
            },
            consent::shell_status(&grants, entry).label()
        );
    }
    text.push_str("hook directories:\n");
    let found = dirs::scan(scope);
    if found.is_empty() {
        text.push_str("  (none)\n");
    }
    for dir in &found {
        let state = dir
            .problem
            .clone()
            .unwrap_or_else(|| dirs::consent(&grants, dir).label().to_owned());
        let _ = writeln!(text, "  {}  {}  {state}", dir.name, dir.events.join(","));
    }
    text.push_str("outbound webhooks:\n");
    let outbound = &scope.config().hooks.outbound;
    if outbound.is_empty() {
        text.push_str("  (none)\n");
    }
    for hook in outbound {
        let signed = if hook.secret_name.is_some() {
            "signed"
        } else {
            "UNSIGNED"
        };
        let _ = writeln!(
            text,
            "  {}  {}  {}  {signed}",
            hook.name,
            hook.url,
            hook.events.join(",")
        );
    }
    print_raw(&text)?;
    Ok(0)
}

/// The synthetic payload of `event`.
fn synthetic(
    event: &str,
    tool: Option<&str>,
    file: Option<&std::path::Path>,
) -> Result<Map<String, Value>, Error> {
    let mut fields = Map::new();
    fields.insert("session_id".to_owned(), json!("ses_hooks_test"));
    if let Some(tool) = tool {
        fields.insert("tool".to_owned(), json!(tool));
        fields.insert("arguments".to_owned(), json!({}));
    }
    if event == "pre_llm_call" {
        fields.insert("user_message".to_owned(), json!("hello"));
    }
    if let Some(path) = file {
        let text =
            std::fs::read_to_string(path).map_err(|e| Error::io(&codes::HOME_IO, path, &e))?;
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(extra)) => fields.extend(extra),
            _ => {
                return Err(Error::new(
                    &codes::CLI_USAGE,
                    format!("{}: the payload must be a JSON object", path.display()),
                ));
            }
        }
    }
    Ok(fields)
}

/// Runs `entry` on `fields`; a missing program is a failure to start.
async fn fire(
    scope: &ProfileScope,
    entry: &ShellEntry,
    fields: &Map<String, Value>,
) -> (Ran, u128) {
    let call = CallInfo {
        session_id: Some("ses_hooks_test".to_owned()),
        surface: "cli".to_owned(),
        cwd: std::env::current_dir()
            .ok()
            .map(|d| d.display().to_string()),
    };
    let stdin = stdin_document(&entry.event, fields, &call, scope.name().as_str());
    let started = Instant::now();
    let ran = match &entry.program {
        Some(program) => {
            ShellRunner::new(
                scope.clone(),
                &entry.event,
                entry.hook.clone(),
                entry.words.clone(),
                program.clone(),
            )
            .run(&stdin, call.cwd.as_deref())
            .await
        }
        None => Ran::NotStarted(format!(
            "program `{}` not found",
            entry.words.first().map_or("", String::as_str)
        )),
    };
    (ran, started.elapsed().as_millis())
}

fn matches(entry: &ShellEntry, event: &str, tool: Option<&str>) -> bool {
    entry.event == event
        && match (&entry.hook.matcher, tool) {
            (Some(matcher), Some(tool)) => {
                regex::Regex::new(matcher).is_ok_and(|r| r.is_match(tool))
            }
            (Some(_), None) => false,
            (None, _) => true,
        }
}

/// The directive as the dispatcher receives it, as JSON.
fn shown(reply: &Result<Option<Reply>, String>) -> Value {
    match reply {
        Ok(None | Some(Reply::None)) => json!({"action": "none"}),
        Ok(Some(Reply::Block(message))) => json!({"action": "block", "message": message}),
        Ok(Some(Reply::Modify(args))) => {
            json!({"action": "modify", "args": serde_json::from_str::<Value>(args).unwrap_or(Value::Null)})
        }
        Ok(Some(Reply::Approve(approval))) => {
            json!({"action": "approve", "message": approval.message, "rule_key": approval.rule_key})
        }
        Ok(Some(Reply::Context(text))) => json!({"context": text}),
        Err(why) => json!({"action": "none", "failed_open": why}),
    }
}

async fn test(
    scope: &ProfileScope,
    event: &str,
    tool: Option<&str>,
    file: Option<&std::path::Path>,
) -> Result<u8, Error> {
    pmt_core::hooks::check(event)?;
    let fields = synthetic(event, tool, file)?;
    let mut text = String::new();
    for entry in consent::shell_entries(scope)
        .iter()
        .filter(|e| matches(e, event, tool))
    {
        let (ran, _) = fire(scope, entry, &fields).await;
        let reply = interpret(
            &entry.event,
            &entry.hook.command,
            entry.hook.fail_closed,
            &ran,
        );
        let _ = writeln!(text, "{}: {}", entry.hook.command, shown(&reply));
    }
    if text.is_empty() {
        let _ = writeln!(text, "no shell hook handles {event}");
    }
    print_raw(&text)?;
    Ok(0)
}

fn allow(
    scope: &ProfileScope,
    event: Option<&str>,
    command: Option<&str>,
    dir: Option<&str>,
) -> Result<u8, Error> {
    let file = GrantsFile::of(scope.profile());
    if let Some(name) = dir {
        let found = dirs::scan(scope).into_iter().find(|d| d.name == name);
        let Some(hook) = found else {
            return Err(Error::new(
                &codes::PLUGINS_NOT_FOUND,
                format!(
                    "no hook directory '{name}' under {}",
                    scope.dir().join("hooks").display()
                ),
            ));
        };
        let digest = hook.digest.clone().ok_or_else(|| {
            Error::new(
                &codes::CONFIG_INVALID,
                format!(
                    "hook directory '{name}' cannot run: {}",
                    hook.problem.clone().unwrap_or_default()
                ),
            )
        })?;
        file.consent(HookConsent {
            kind: HookKind::Directory,
            event: None,
            subject: name.to_owned(),
            digest,
            granted_at: jiff::Timestamp::now(),
        })?;
        eprintln!("approved hook directory '{name}'");
        return Ok(0);
    }
    let (event, command) = (event.unwrap_or_default(), command.unwrap_or_default());
    let entry = consent::shell_entries(scope)
        .into_iter()
        .find(|e| e.event == event && e.hook.command == command)
        .ok_or_else(|| {
            Error::new(
                &codes::PLUGINS_NOT_FOUND,
                format!("no [[hooks.{event}]] entry with command `{command}` in config.toml"),
            )
        })?;
    consent::approve_shell(&file, &entry)?;
    eprintln!("approved shell hook `{command}` on {event}");
    Ok(0)
}

fn revoke(scope: &ProfileScope, command: &str) -> Result<u8, Error> {
    let removed = GrantsFile::of(scope.profile()).revoke_hooks(|c| c.subject == command)?;
    if removed.is_empty() {
        return Err(Error::new(
            &codes::PLUGINS_NOT_FOUND,
            format!("no approval of hook `{command}`"),
        ));
    }
    eprintln!("revoked {} approval(s) of `{command}`", removed.len());
    Ok(0)
}

async fn doctor(scope: &ProfileScope) -> Result<u8, Error> {
    let grants = GrantsFile::of(scope.profile()).load()?;
    let mut text = String::new();
    let mut failed = false;
    for entry in consent::shell_entries(scope) {
        let (problems, timing) = shell_problems(scope, &grants, &entry).await?;
        failed |= !problems.is_empty();
        let _ = writeln!(
            text,
            "{} {}: {}{timing}",
            entry.event,
            entry.hook.command,
            verdict(&problems)
        );
    }
    for dir in dirs::scan(scope) {
        let problems = dir_problems(&grants, &dir);
        failed |= !problems.is_empty();
        let _ = writeln!(text, "hooks/{}: {}", dir.name, verdict(&problems));
    }
    if text.is_empty() {
        text.push_str("no hooks\n");
    }
    print_raw(&text)?;
    Ok(u8::from(failed))
}

fn verdict(problems: &[String]) -> String {
    if problems.is_empty() {
        "ok".to_owned()
    } else {
        problems.join("; ")
    }
}

/// A shell hook's problems and, when it ran, its run time.
async fn shell_problems(
    scope: &ProfileScope,
    grants: &pmt_config::grants::Grants,
    entry: &ShellEntry,
) -> Result<(Vec<String>, String), Error> {
    let mut problems = Vec::new();
    match &entry.program {
        None => problems.push("program not found".to_owned()),
        Some(program) if !executable(program) => problems.push("not executable".to_owned()),
        Some(_) => {}
    }
    let state = consent::shell_status(grants, entry);
    if state != Consent::Approved && state != Consent::Missing {
        problems.push(state.label().to_owned());
    }
    if entry.program.is_none() {
        return Ok((problems, String::new()));
    }
    let fields = synthetic(&entry.event, Some("terminal"), None)?;
    let (ran, ms) = fire(scope, entry, &fields).await;
    match &ran {
        Ran::Exited {
            code: Some(0),
            stdout,
            ..
        } if !stdout.trim().is_empty() && serde_json::from_str::<Value>(stdout.trim()).is_err() => {
            problems.push("stdout is not JSON".to_owned());
        }
        Ran::Exited {
            code: Some(0 | 2), ..
        } => {}
        other => problems.push(format!("{other:?}")),
    }
    Ok((problems, format!(" ({ms} ms)")))
}

fn dir_problems(grants: &pmt_config::grants::Grants, dir: &dirs::HookDir) -> Vec<String> {
    let mut problems: Vec<String> = dir.problem.iter().cloned().collect();
    if dir.handler.as_deref().is_some_and(|h| !executable(h)) {
        problems.push("handler not executable".to_owned());
    }
    let state = dirs::consent(grants, dir);
    if state != Consent::Approved && state != Consent::Missing {
        problems.push(state.label().to_owned());
    }
    problems
}

#[cfg(unix)]
fn executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(path: &std::path::Path) -> bool {
    path.is_file()
}
