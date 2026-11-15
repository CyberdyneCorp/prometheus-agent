//! Harness hooks (spec: harness-evolution, Harness execution
//! confinement): the `[[hooks]]` of the active version's `hooks.toml` run
//! like shell hooks, in the profile's process sandbox under its baseline
//! grants (reading the harness scripts they name, nothing more). A harness
//! cannot declare anything that widens that: validation refuses every
//! other key. They need no first-use consent, because a version with
//! hooks is only ever activated by the user (an edit, a rollback, or an
//! adoption, which always asks when hooks change).
//!
//! A harness-evolution rollout runs with its candidate's hooks instead
//! (spec: Isolated rollouts): [`Rollout`] is a hook source over the
//! profile's set with the active version's hooks swapped for the
//! candidate's, so a candidate's hook edits are what its rollouts test.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use pmt_config::ProfileScope;
use pmt_config::harness::{HarnessDir, Tree};
use pmt_config::schema::ShellHook;
use pmt_plugins::hooks::{HookSource, Hooks};

use crate::hooks_host::consent::{ShellEntry, program};
use crate::hooks_host::shell::{ShellRunner, external};

/// What harness hook callbacks are labelled with, before the command:
/// how a rollout finds the active version's hooks to replace.
const LABEL: &str = "harness:";

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
    entries_of(scope, &tree, &harness.tree_dir(&active))
}

/// The shell entries of `tree`'s hooks, its scripts under `root`.
fn entries_of(scope: &ProfileScope, tree: &Tree, root: &Path) -> Vec<ShellEntry> {
    pmt_evolve::harness::hooks_of(tree)
        .into_iter()
        .filter_map(|hook| entry(scope, root, hook))
        .collect()
}

/// A runnable entry: harness scripts made absolute inside the version's
/// tree, the program resolved (an interpreter on `PATH`, or the script).
fn entry(
    scope: &ProfileScope,
    root: &Path,
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
    add_entries(scope, entries(scope), hooks);
}

fn add_entries(scope: &ProfileScope, entries: Vec<ShellEntry>, hooks: &mut Hooks) {
    for entry in entries {
        let Some(program) = entry.program.clone() else {
            continue;
        };
        let event = entry.event.clone();
        let label = format!("{LABEL}{}", entry.hook.command);
        let runner = ShellRunner::new(scope.clone(), &entry, program).labelled(label);
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

/// The hook set of a rollout session: the profile's current set (it
/// follows plugin reloads) with the active harness's hooks replaced by
/// those of the candidate tree in `harness`, run by the same sandboxed
/// runner under the same grants. Safe mode keeps every harness hook out,
/// as it does for the profile's set.
pub struct Rollout {
    profile: Arc<dyn HookSource>,
    scope: ProfileScope,
    entries: Vec<ShellEntry>,
    /// The profile set last derived from, and the set derived.
    derived: Mutex<Option<(Arc<Hooks>, Arc<Hooks>)>>,
}

impl Rollout {
    /// The rollout hook source over `profile` for the candidate tree in
    /// `harness`.
    #[must_use]
    pub fn new(scope: &ProfileScope, profile: Arc<dyn HookSource>, harness: &Path) -> Self {
        let entries = if scope.config().plugins.safe_mode {
            Vec::new()
        } else {
            let (tree, _) = Tree::read(harness).unwrap_or_default();
            entries_of(scope, &tree, harness)
        };
        Self {
            profile,
            scope: scope.clone(),
            entries,
            derived: Mutex::new(None),
        }
    }
}

impl HookSource for Rollout {
    fn hooks(&self) -> Arc<Hooks> {
        let base = self.profile.hooks();
        let mut derived = self.derived.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((from, hooks)) = derived.as_ref()
            && Arc::ptr_eq(from, &base)
        {
            return Arc::clone(hooks);
        }
        let mut hooks = base.without_external(|label| label.starts_with(LABEL));
        add_entries(&self.scope, self.entries.clone(), &mut hooks);
        let hooks = Arc::new(hooks);
        *derived = Some((base, Arc::clone(&hooks)));
        hooks
    }
}
