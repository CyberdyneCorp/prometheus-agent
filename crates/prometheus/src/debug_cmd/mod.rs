//! `prometheus debug` (spec: observability, Plain-text setup summary;
//! Crash and debug bundle; Opt-in debug share upload; Runtime liveness
//! watchdog and task dump): `summary [--show-key-prefixes]` prints one
//! plain-text block for a support thread (secret values never appear;
//! with `--show-key-prefixes`, only the first and last four characters of
//! secrets longer than 16 characters; credentials embedded in URLs become
//! `***`), `bundle` writes a redacted local archive, `share` uploads a
//! redacted report only after an explicit yes, `delete` removes a recorded
//! share, and `dump-tasks` asks the daemon for a task dump.

pub mod bundle;
pub mod share;

use std::fmt::Write as _;
use std::path::PathBuf;

use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_core::runtime::{Role, live_record};
use pmt_sandbox::{Paths, Policy, Sandbox};

use crate::cli::DebugCommand;
use crate::context::Context;
use crate::output::print_raw;
use crate::setup_cmd::sections::required_secrets;

/// Runs `prometheus debug`.
///
/// # Errors
///
/// Home, profile and configuration errors.
pub fn run(command: &DebugCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        DebugCommand::Summary { show_key_prefixes } => {
            let scope = ctx.scope()?;
            print_raw(&summary(ctx, &scope, *show_key_prefixes))
        }
        DebugCommand::Bundle(args) => bundle::run(args, ctx),
        DebugCommand::Share(args) => share::run(args, ctx),
        DebugCommand::Delete { url } => share::delete(url, ctx),
        DebugCommand::DumpTasks => dump_tasks(ctx),
    }
}

/// `prometheus debug dump-tasks`: the running daemon writes a task dump
/// without stopping; its path.
fn dump_tasks(ctx: &Context<'_>) -> Result<u8, Error> {
    let home = ctx.home()?.root().to_path_buf();
    let Some(record) = (match pmt_rpc::discovery::discover(&home)? {
        pmt_rpc::discovery::Discovery::Running(record) => Some(record),
        _ => None,
    }) else {
        return Err(Error::new(
            &pmt_core::error::codes::DAEMON_NOT_RUNNING,
            "the daemon is not running; there are no tasks to dump",
        ));
    };
    let control = record.endpoint.control.clone();
    let response = crate::model_cmd::block_on(async move {
        pmt_rpc::control::call(&control, &pmt_rpc::control::ControlRequest::DumpTasks).await
    })??;
    if let Some(error) = response.error {
        return Err(Error::new(&pmt_core::error::codes::DEBUG_IO, error.message));
    }
    let path = response.dumped.unwrap_or_default();
    if ctx.json {
        return crate::output::document("debug dump-tasks", &serde_json::json!({ "path": path }));
    }
    crate::output::print(&format!("task dump written to {path}"))
}

/// Replaces `user:password@` in every URL of `text` with `***@`.
#[must_use]
pub fn redact_url_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (head, tail) = rest.split_at(at + 3);
        out.push_str(head);
        let authority_end = tail
            .find(|c: char| c == '/' || c == '?' || c == '#' || c.is_whitespace() || c == '"')
            .unwrap_or(tail.len());
        match tail[..authority_end].rfind('@') {
            Some(credentials) => {
                out.push_str("***");
                rest = &tail[credentials..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

/// `abcd…wxyz` for a secret longer than 16 characters, else `set`.
#[must_use]
pub fn key_prefix(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 16 {
        return "set".to_owned();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// The plain-text setup summary of `scope` (`debug summary`, the head of
/// a `debug share` report).
pub(crate) fn summary(ctx: &Context<'_>, scope: &ProfileScope, prefixes: bool) -> String {
    let config = scope.config();
    let mut text = String::new();
    let mut line = |label: &str, value: &str| {
        let _ = writeln!(text, "{label:<22}{value}");
    };
    line(
        "version",
        &format!(
            "prometheus {} ({}, {}, {})",
            crate::build_info::VERSION,
            crate::build_info::channel(crate::build_info::VERSION),
            crate::build_info::TARGET,
            crate::build_info::COMMIT
        ),
    );
    line(
        "os",
        &format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
    );
    line("install owner", &install_owner(scope));
    line(
        "profile",
        &format!("{} (home {})", scope.name(), scope.home().display()),
    );
    let model = &config.model;
    line(
        "model",
        &format!(
            "{} / provider {}",
            model.default.as_deref().unwrap_or("(provider default)"),
            model.provider.as_deref().unwrap_or("auto")
        ),
    );
    line(
        "terminal",
        &format!(
            "backend {}, sandbox {}",
            config.terminal.backend,
            sandbox_enforcement(ctx, scope)
        ),
    );
    line("secrets", &secrets(scope, prefixes));
    line("toolsets", &config.tools.enabled_toolsets.join(", "));
    line("mcp servers", "0");
    line("memory provider", "builtin");
    line("daemon", &daemon(scope));
    line("scheduled jobs", "0");
    line("skills", &skill_count(ctx, scope).to_string());
    let changed = changed_keys(scope);
    let _ = writeln!(text, "changed config keys:");
    if changed.is_empty() {
        let _ = writeln!(text, "  (none)");
    }
    for (key, value) in changed {
        let _ = writeln!(text, "  {key} = {value}");
    }
    let redactor = scope.secrets().redactor().unwrap_or_default();
    redact_url_credentials(&redactor.redact_all(&text))
}

fn install_owner(scope: &ProfileScope) -> String {
    let Ok(binary) = std::env::current_exe() else {
        return "unknown".to_owned();
    };
    match pmt_update::install::InstallReceipt::read(&binary, scope.home()) {
        Ok(Some(receipt)) => receipt.owner.as_str().to_owned(),
        Ok(None) => "unknown (no install receipt)".to_owned(),
        Err(_) => "unknown (unreadable install receipt)".to_owned(),
    }
}

/// What the OS sandbox enforces for `scope` here (`debug summary`,
/// `status --full`).
pub(crate) fn sandbox_enforcement(ctx: &Context<'_>, scope: &ProfileScope) -> String {
    let scratch = scope.path("cache/scratch");
    let paths = Paths {
        user_home: ctx
            .launch
            .user_home()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("/")),
        prometheus_home: scope.home().to_path_buf(),
        workspace: std::env::current_dir().unwrap_or_else(|_| scratch.clone()),
        scratch: vec![scratch],
    };
    Policy::baseline(&paths)
        .and_then(|policy| Sandbox::new(policy, &scope.config().security.sandbox))
        .map_or_else(
            |err| format!("unavailable ({})", err.message()),
            |sandbox| sandbox.report().enforcement().to_string(),
        )
}

fn secrets(scope: &ProfileScope, prefixes: bool) -> String {
    let mut parts = Vec::new();
    let mut names = scope.secrets().store().names().unwrap_or_default();
    for required in required_secrets(scope).unwrap_or_default() {
        if !names.contains(&required.name) {
            names.push(required.name);
        }
    }
    names.sort();
    for name in names {
        let state = match scope.secrets().resolve(&name) {
            Ok(value) if prefixes => key_prefix(value.expose()),
            Ok(_) => "set".to_owned(),
            Err(_) => "missing".to_owned(),
        };
        parts.push(format!("{name} {state}"));
    }
    if parts.is_empty() {
        "(none)".to_owned()
    } else {
        parts.join(", ")
    }
}

fn daemon(scope: &ProfileScope) -> String {
    match live_record(scope.home(), Role::Daemon) {
        Ok(Some(record)) => format!("running (pid {}), adapters: none", record.pid),
        Ok(None) => "not running".to_owned(),
        Err(err) => format!("unknown ({})", err.message()),
    }
}

fn skill_count(ctx: &Context<'_>, scope: &ProfileScope) -> usize {
    let env = crate::skills_cmd::Env {
        scope: scope.clone(),
        user_home: ctx.launch.user_home().cloned(),
        cwd: None,
        bundled: pmt_skills::bundled::Bundled::builtin(),
        json: false,
        launch: ctx.launch.clone(),
    };
    env.profile_catalog().skills.len()
}

/// The keys whose effective value differs from the schema default.
fn changed_keys(scope: &ProfileScope) -> Vec<(String, String)> {
    let layers = scope.layers();
    layers
        .changed()
        .into_iter()
        .filter_map(|key| {
            let value = layers.value(&key)?.to_string();
            Some((key, value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_credentials_are_replaced() {
        assert_eq!(
            redact_url_credentials(
                "base = \"https://user:pa55@host.test:8080/v1\" and http://plain.test/x"
            ),
            "base = \"https://***@host.test:8080/v1\" and http://plain.test/x"
        );
        assert_eq!(redact_url_credentials("no urls"), "no urls");
    }

    #[test]
    fn key_prefixes_only_for_long_secrets() {
        assert_eq!(key_prefix("short-secret"), "set");
        assert_eq!(key_prefix("sk-ant-0123456789abcdefXYZ1"), "sk-a…XYZ1");
    }
}
