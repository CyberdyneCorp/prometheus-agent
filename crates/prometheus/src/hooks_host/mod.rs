//! Hooks that are not plugins (spec: plugin-system, Shell hooks with
//! first-use consent; Gateway hook directories; Outbound webhooks): they
//! join a profile's hook set after its plugin callbacks, so plugin hooks
//! always run first, and only once approved (shell hooks and hook
//! directories) or configured (webhooks). Safe mode registers none of
//! them.

pub mod consent;
pub mod dirs;
pub mod outbound;
pub mod shell;

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_config::grants::{GrantsFile, baseline};
use pmt_core::capability::Capability;
use pmt_core::error::Error;
use pmt_plugins::hooks::Hooks;
use pmt_sandbox::{Paths, Policy, Sandbox};

use self::consent::Consent;

/// A command for `program` under the profile's process sandbox: the
/// capability baseline plus reading the directories of the program and of
/// the `scripts` its arguments name, with the session's working directory
/// (`cwd`, else the profile home) as the workspace.
///
/// # Errors
///
/// `sandbox.unavailable` / `sandbox.setup`.
pub fn sandboxed(
    scope: &ProfileScope,
    program: &Path,
    scripts: &[std::path::PathBuf],
    cwd: Option<&str>,
) -> Result<Command, Error> {
    let config = scope.config();
    let scratch = scope.dir().join("cache/hooks");
    let _ = std::fs::create_dir_all(&scratch);
    let workspace = cwd.map_or_else(|| scope.dir().to_path_buf(), Into::into);
    let paths = Paths {
        user_home: dirs_home().unwrap_or_default(),
        prometheus_home: scope.home().to_path_buf(),
        workspace: workspace.clone(),
        scratch: vec![scratch.clone()],
    };
    let mut extra = vec![Capability::ProcessSpawn("*".to_owned())];
    let files = std::iter::once(program).chain(scripts.iter().map(std::path::PathBuf::as_path));
    for dir in files.filter_map(Path::parent) {
        extra.push(Capability::FsRead(format!("{}/**", dir.display())));
    }
    let capabilities = baseline(&config.security).union(&extra.into_iter().collect());
    let sandbox = Sandbox::new(
        Policy::from_set(&capabilities, &paths),
        &config.security.sandbox,
    )?;
    let mut command = sandbox.command(program)?;
    command.current_dir(&workspace).env_clear();
    for var in ["PATH", "HOME", "LANG", "SYSTEMROOT"] {
        if let Some(value) = std::env::var_os(var) {
            command.env(var, value);
        }
    }
    // The one place a hook may write besides the workspace.
    command.env("TMPDIR", &scratch);
    Ok(command)
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(Into::into)
}

/// Adds the profile's approved shell hooks, approved gateway hook
/// directories and outbound webhooks to `hooks`, after the plugins'.
/// Unapproved hooks are skipped with a warning naming the profile, unless
/// `hooks.auto_accept` accepts them.
pub fn extend(scope: &ProfileScope, hooks: &mut Hooks) {
    if scope.config().plugins.safe_mode {
        return;
    }
    let grants = GrantsFile::of(scope.profile()).load().unwrap_or_default();
    add_shell(scope, &grants, hooks);
    add_dirs(scope, &grants, hooks);
    for (events, target) in outbound::targets(scope) {
        for event in events {
            let _ = hooks.add_external(&event, Arc::clone(&target));
        }
    }
}

fn add_shell(scope: &ProfileScope, grants: &pmt_config::grants::Grants, hooks: &mut Hooks) {
    let accept = scope.config().hooks.auto_accept;
    let profile = scope.name().as_str();
    for entry in consent::shell_entries(scope) {
        let state = consent::shell_status(grants, &entry);
        let runnable = state == Consent::Approved || (accept && state != Consent::Missing);
        let Some(program) = entry.program.clone().filter(|_| runnable) else {
            tracing::warn!(
                profile,
                event = %entry.event,
                "shell hook `{}` skipped: {} (approve it with `prometheus plugins hooks allow {} '{}'`)",
                entry.hook.command,
                state.label(),
                entry.event,
                entry.hook.command
            );
            continue;
        };
        let event = entry.event.clone();
        let runner = shell::ShellRunner::new(scope.clone(), &entry, program);
        if let Err(err) = hooks.add_external(&event, shell::external(runner)) {
            tracing::warn!(profile, "shell hook skipped: {}", err.message());
        }
    }
}

fn add_dirs(scope: &ProfileScope, grants: &pmt_config::grants::Grants, hooks: &mut Hooks) {
    let profile = scope.name().as_str();
    for dir in dirs::scan(scope) {
        let state = dirs::consent(grants, &dir);
        if dir.problem.is_some() || state != Consent::Approved {
            tracing::warn!(
                profile,
                hook = %dir.name,
                "hook directory skipped: {}",
                dir.problem.clone().unwrap_or_else(|| state.label().to_owned())
            );
            continue;
        }
        let events = dir.events.clone();
        let runner: Arc<dyn pmt_plugins::hooks::ExternalHook> =
            Arc::new(dirs::DirRunner::new(scope.clone(), dir));
        for event in events {
            let _ = hooks.add_external(&event, Arc::clone(&runner));
        }
    }
}

/// What decides whether the non-plugin hooks changed: `[hooks]`, the
/// programs' and handlers' digests and the grants file (part of the
/// plugin set's fingerprint, so a change reloads the set).
#[must_use]
pub fn fingerprint(scope: &ProfileScope) -> String {
    let mut text = format!("{:?}|", scope.config().hooks);
    for entry in consent::shell_entries(scope) {
        // The digest covers the program and every script argument, so
        // editing `guard.py` of `python3 guard.py` reloads the set.
        let _ = write!(text, "{}={:?};", entry.hook.command, entry.digest);
    }
    for dir in dirs::scan(scope) {
        let _ = write!(text, "{}={:?}:{:?};", dir.name, dir.digest, dir.events);
    }
    text
}
