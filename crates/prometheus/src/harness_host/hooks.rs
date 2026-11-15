//! Harness hooks (spec: harness-evolution, Harness execution
//! confinement): the `[[hooks]]` of the active version's `hooks.toml` run
//! like shell hooks, in the profile's process sandbox under its baseline
//! grants (reading the harness scripts they name, nothing more). A harness
//! cannot declare anything that widens that: validation refuses every
//! other key. They need no first-use consent, because a version with
//! hooks is only ever activated by the user (an edit, a rollback, or an
//! adoption, which always asks when hooks change).

use std::path::PathBuf;

use pmt_config::ProfileScope;
use pmt_config::harness::HarnessDir;
use pmt_config::schema::ShellHook;
use pmt_plugins::hooks::Hooks;

use crate::hooks_host::consent::{ShellEntry, program};
use crate::hooks_host::shell::{ShellRunner, external};

/// The shell entries of the active version's hooks.
#[must_use]
pub fn entries(scope: &ProfileScope) -> Vec<ShellEntry> {
    let harness = HarnessDir::of(scope.dir());
    let Ok(active) = harness.active_id() else {
        return Vec::new();
    };
    let Ok(tree) = harness.tree(&active) else {
        return Vec::new();
    };
    let root = harness.tree_dir(&active);
    pmt_evolve::harness::hooks_of(&tree)
        .into_iter()
        .filter_map(|hook| entry(scope, &root, hook))
        .collect()
}

/// A runnable entry: harness scripts made absolute inside the version's
/// tree, the program resolved (an interpreter on `PATH`, or the script).
fn entry(
    scope: &ProfileScope,
    root: &std::path::Path,
    hook: pmt_evolve::harness::HarnessHook,
) -> Option<ShellEntry> {
    let mut words = shlex::split(&hook.command)?;
    let mut scripts = Vec::new();
    for word in &mut words {
        let relative = word.trim_start_matches("./");
        let file = root.join(relative);
        if relative.contains('/') && file.is_file() {
            *word = file.display().to_string();
            scripts.push(file);
        }
    }
    let first = words.first()?.clone();
    let program: PathBuf = if scripts
        .first()
        .is_some_and(|s| s.display().to_string() == first)
    {
        PathBuf::from(&first)
    } else {
        program(scope, &first)?
    };
    Some(ShellEntry {
        event: hook.event,
        hook: ShellHook {
            command: hook.command,
            matcher: hook.matcher,
            timeout_secs: hook.timeout_secs,
            fail_closed: false,
        },
        words,
        program: Some(program),
        scripts,
        digest: None,
    })
}

/// Adds the active harness's hooks to `hooks`.
pub fn add(scope: &ProfileScope, hooks: &mut Hooks) {
    for entry in entries(scope) {
        let Some(program) = entry.program.clone() else {
            continue;
        };
        let event = entry.event.clone();
        let runner = ShellRunner::new(scope.clone(), &entry, program);
        if let Err(err) = hooks.add_external(&event, external(runner)) {
            tracing::warn!(profile = %scope.name(), "harness hook skipped: {}", err.message());
        }
    }
}

/// What decides whether the harness hooks changed: the active version.
#[must_use]
pub fn fingerprint(scope: &ProfileScope) -> String {
    HarnessDir::of(scope.dir()).active_id().unwrap_or_default()
}
