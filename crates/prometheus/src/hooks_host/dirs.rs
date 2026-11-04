//! Gateway hook directories (spec: plugin-system, Gateway hook
//! directories): `<profile_home>/hooks/<name>/` with `HOOK.toml` (`name`,
//! `description`, non-empty `events` from the closed hook registry) and an
//! executable `handler` run with the shell-hook JSON protocol; a Hermes
//! `HOOK.yaml` is translated, its event names mapped onto the registry.
//!
//! Placing a directory is not consent: its handler runs only once its
//! digest is approved in `grants.toml` (`prometheus plugins hooks allow
//! --dir <name>`). Handlers run sandboxed and fire-and-forget (errors are
//! logged, never blocking the gateway), and only for gateway events: the
//! CLI, TUI, desktop and cron never run them.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_config::grants::{Grants, HookKind};
use pmt_plugins::hooks::{ExternalHook, HookFuture, Payload};
use pmt_plugins::host::CallInfo;
use serde::Deserialize;
use serde_json::Value;

use super::consent::{Consent, file_digest, status};
use super::shell::{run_child, stdin_document};

/// How long a handler may run.
const HANDLER_TIMEOUT: Duration = Duration::from_mins(1);

/// Hermes event names and the registry names they map onto (spec table).
const HERMES_EVENTS: &[(&str, &str)] = &[
    ("gateway:startup", "on_gateway_startup"),
    ("session:start", "on_session_start"),
    ("session:end", "on_session_end"),
    ("session:reset", "on_session_reset"),
    ("session:compress", "post_compaction"),
    ("agent:start", "on_turn_start"),
    ("agent:step", "post_llm_call"),
    ("agent:end", "on_turn_end"),
    ("reaction:added", "gateway_platform_event"),
    ("reaction:removed", "gateway_platform_event"),
];

/// One hook directory as found on disk.
#[derive(Debug, Clone)]
pub struct HookDir {
    /// The directory name.
    pub name: String,
    /// Its directory.
    pub dir: PathBuf,
    /// `description`.
    pub description: String,
    /// The registry events it handles.
    pub events: Vec<String>,
    /// `pre_command` only for these command names (Hermes `command:<n>`).
    pub commands: Vec<String>,
    /// Its executable handler.
    pub handler: Option<PathBuf>,
    /// `sha256:<hex>` of the handler.
    pub digest: Option<String>,
    /// Why it cannot run (a bad manifest, no handler).
    pub problem: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    name: String,
    #[serde(default)]
    description: String,
    events: Vec<String>,
}

/// Every directory under `<profile_home>/hooks/`, sorted by name.
#[must_use]
pub fn scan(scope: &ProfileScope) -> Vec<HookDir> {
    let root = scope.dir().join("hooks");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut dirs: Vec<HookDir> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| read(&e.path()))
        .collect();
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    dirs
}

fn read(dir: &Path) -> HookDir {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut hook = HookDir {
        name,
        dir: dir.to_path_buf(),
        description: String::new(),
        events: Vec::new(),
        commands: Vec::new(),
        handler: None,
        digest: None,
        problem: None,
    };
    match manifest(dir) {
        Ok((manifest, _, _)) if manifest.name != hook.name => {
            hook.problem = Some(format!(
                "its name '{}' does not match its directory '{}'",
                manifest.name, hook.name
            ));
        }
        Ok((manifest, events, commands)) => {
            hook.description = manifest.description;
            hook.events = events;
            hook.commands = commands;
        }
        Err(problem) => hook.problem = Some(problem),
    }
    let handler = dir.join("handler");
    if handler.is_file() {
        hook.digest = file_digest(&handler);
        hook.handler = Some(handler);
    } else if dir.join("handler.py").is_file() {
        hook.problem.get_or_insert_with(|| {
            "handler.py needs the python bridge, which this build does not host yet".to_owned()
        });
    } else {
        hook.problem
            .get_or_insert_with(|| "no executable `handler` in the directory".to_owned());
    }
    hook
}

type Parsed = (Manifest, Vec<String>, Vec<String>);

fn manifest(dir: &Path) -> Result<Parsed, String> {
    if let Ok(text) = std::fs::read_to_string(dir.join("HOOK.toml")) {
        let manifest: Manifest = toml::from_str(&text).map_err(|e| format!("HOOK.toml: {e}"))?;
        if manifest.events.is_empty() {
            return Err("HOOK.toml: events is empty".to_owned());
        }
        if let Some(bad) = manifest
            .events
            .iter()
            .find(|e| pmt_core::hooks::check(e).is_err())
        {
            return Err(format!("HOOK.toml: unknown hook '{bad}'"));
        }
        let events = manifest.events.clone();
        return Ok((manifest, events, Vec::new()));
    }
    let text = std::fs::read_to_string(dir.join("HOOK.yaml"))
        .map_err(|_| "no HOOK.toml (or Hermes HOOK.yaml)".to_owned())?;
    let manifest = hermes_yaml(&text)?;
    let (events, commands) = translate(&manifest.events)?;
    Ok((manifest, events, commands))
}

/// The keys of a Hermes `HOOK.yaml` (`name`, `description`, `events` as a
/// block or flow list).
fn hermes_yaml(text: &str) -> Result<Manifest, String> {
    let mut name = None;
    let mut description = String::new();
    let mut events = Vec::new();
    let mut in_events = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if in_events && let Some(item) = trimmed.strip_prefix("- ") {
            events.push(unquote(item));
            continue;
        }
        in_events = false;
        let Some((key, value)) = trimmed.split_once(':') else {
            return Err(format!("HOOK.yaml: cannot read `{trimmed}`"));
        };
        let value = value.trim();
        match key.trim() {
            "name" => name = Some(unquote(value)),
            "description" => description = unquote(value),
            "events" if value.is_empty() => in_events = true,
            "events" => {
                let inner = value.trim_start_matches('[').trim_end_matches(']');
                events.extend(inner.split(',').map(unquote).filter(|e| !e.is_empty()));
            }
            _ => {}
        }
    }
    if events.is_empty() {
        return Err("HOOK.yaml: events is empty".to_owned());
    }
    Ok(Manifest {
        name: name.ok_or("HOOK.yaml: no name")?,
        description,
        events,
    })
}

fn unquote(text: &str) -> String {
    text.trim().trim_matches(['"', '\'']).to_owned()
}

/// Hermes event names onto the registry: the mapped names (deduplicated)
/// and the command names of `command:<name>` entries.
///
/// # Errors
///
/// A name that maps onto nothing.
pub fn translate(hermes: &[String]) -> Result<(Vec<String>, Vec<String>), String> {
    let mut events: Vec<String> = Vec::new();
    let mut commands = Vec::new();
    let mut add = |event: &str| {
        if !events.iter().any(|e| e == event) {
            events.push(event.to_owned());
        }
    };
    for name in hermes {
        if let Some(command) = name.strip_prefix("command:") {
            if command != "*" {
                commands.push(command.to_owned());
            }
            add("pre_command");
        } else if let Some(prefix) = name.strip_suffix(":*") {
            let mapped: Vec<&str> = HERMES_EVENTS
                .iter()
                .filter(|(h, _)| h.split(':').next() == Some(prefix))
                .map(|(_, ours)| *ours)
                .collect();
            if mapped.is_empty() {
                return Err(format!("HOOK.yaml: unknown event '{name}'"));
            }
            mapped.into_iter().for_each(&mut add);
        } else if let Some((_, ours)) = HERMES_EVENTS.iter().find(|(h, _)| h == name) {
            add(ours);
        } else if pmt_core::hooks::check(name).is_ok() {
            add(name);
        } else {
            return Err(format!("HOOK.yaml: unknown event '{name}'"));
        }
    }
    Ok((events, commands))
}

/// Where `hook` stands.
#[must_use]
pub fn consent(grants: &Grants, hook: &HookDir) -> Consent {
    match &hook.digest {
        Some(digest) => status(grants, HookKind::Directory, None, &hook.name, digest),
        None => Consent::Missing,
    }
}

/// An approved hook directory's handler, run for gateway events.
pub struct DirRunner {
    label: String,
    hook: HookDir,
    scope: ProfileScope,
}

impl DirRunner {
    /// A runner of the approved `hook`.
    #[must_use]
    pub fn new(scope: ProfileScope, hook: HookDir) -> Self {
        Self {
            label: format!("hooks/{}", hook.name),
            hook,
            scope,
        }
    }
}

impl ExternalHook for DirRunner {
    fn label(&self) -> &str {
        &self.label
    }

    fn applies(&self, hook: &str, payload: &Payload, call: &CallInfo) -> bool {
        if call.surface != "gateway" {
            return false;
        }
        if hook != "pre_command" || self.hook.commands.is_empty() {
            return true;
        }
        payload
            .fields()
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|c| {
                self.hook
                    .commands
                    .iter()
                    .any(|n| n == c.trim_start_matches('/'))
            })
    }

    fn call<'a>(
        &'a self,
        hook: &'a str,
        payload: &'a Payload,
        call: &'a CallInfo,
    ) -> HookFuture<'a> {
        let stdin = stdin_document(hook, payload.fields(), call, self.scope.name().as_str());
        let handler = self.hook.handler.clone();
        let scope = self.scope.clone();
        let label = self.label.clone();
        let cwd = call.cwd.clone();
        let event = hook.to_owned();
        // Fire and forget: the gateway never waits for a handler.
        tokio::spawn(async move {
            let Some(handler) = handler else {
                return;
            };
            let command = match super::sandboxed(&scope, &handler, &[], cwd.as_deref()) {
                Ok(command) => command,
                Err(err) => {
                    tracing::warn!(hook = %label, event, "hook handler not started: {}", err.message());
                    return;
                }
            };
            let mut command = tokio::process::Command::from(command);
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let ran = run_child(command, &stdin, HANDLER_TIMEOUT).await;
            if !matches!(ran, super::shell::Ran::Exited { code: Some(0), .. }) {
                tracing::warn!(hook = %label, event, "hook handler failed: {ran:?}");
            }
        });
        Box::pin(async { Ok(None) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermes_events_map_onto_the_registry_and_wildcards_expand() {
        let names: Vec<String> = ["session:*", "agent:end", "command:deploy", "reaction:added"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let (events, commands) = translate(&names).unwrap();
        assert_eq!(
            events,
            [
                "on_session_start",
                "on_session_end",
                "on_session_reset",
                "post_compaction",
                "on_turn_end",
                "pre_command",
                "gateway_platform_event"
            ]
        );
        assert_eq!(commands, ["deploy"]);
        assert!(translate(&["nope:thing".to_owned()]).is_err());
    }

    #[test]
    fn a_hermes_hook_yaml_is_read() {
        let manifest = hermes_yaml(
            "name: alert\ndescription: \"Pages\"\nevents:\n  - agent:end\n  - gateway:startup\n",
        )
        .unwrap();
        assert_eq!(manifest.name, "alert");
        assert_eq!(manifest.events, ["agent:end", "gateway:startup"]);
    }
}
