//! The harness of running sessions (spec: harness-evolution, Harness
//! loaded once per session; Harness execution confinement).
//!
//! A session resolves `<profile>/harness/ACTIVE` once, records the version
//! on its row and keeps it for life (a resumed session reads it back): its
//! prompt carries that version's always-on files as the harness layer, and
//! its skills and subagents include that version's. Only an explicit
//! `--now` activation ([`now`]) reaches an open session, at its next
//! prompt rebuild. Harness hooks join the profile's hook set ([`hooks`]),
//! sandboxed under the session's grants; each session's view swaps in the
//! hooks of its own version.

pub mod hooks;
pub mod now;

use pmt_config::ProfileScope;
use pmt_config::harness::{ALWAYS_ON, HarnessDir, Tree};
use pmt_core::id::SessionId;
use pmt_evolve::harness::Rules;
use pmt_store::Store;

/// The harness version session `id` runs on: the one it recorded, else
/// the active one, recorded now. `None` (logged) when the harness cannot
/// be read; the session then runs without a harness layer.
#[must_use]
pub fn session_version(scope: &ProfileScope, store: &Store, id: SessionId) -> Option<String> {
    match store.session_harness(id) {
        Ok(Some(version)) => return Some(version),
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(session = %id, error = %err, "harness version of the session unreadable");
            return None;
        }
    }
    let version = HarnessDir::of(scope.dir())
        .active_id()
        .inspect_err(|err| tracing::warn!(error = %err, "harness not loaded"))
        .ok()?;
    if let Err(err) = store.set_session_harness(id, &version) {
        tracing::warn!(session = %id, error = %err, "harness version not recorded");
    }
    Some(version)
}

/// The prompt layer of `tree`: its non-empty always-on files, c1 to c4.
#[must_use]
pub fn layer_of(tree: &Tree) -> Option<String> {
    let parts: Vec<&str> = ALWAYS_ON
        .iter()
        .filter_map(|p| tree.get(p))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join(pmt_agent::prompt::SEPARATOR))
}

/// The prompt layer of `version`.
#[must_use]
pub fn layer(scope: &ProfileScope, version: &str) -> Option<String> {
    match HarnessDir::of(scope.dir()).tree(version) {
        Ok(tree) => layer_of(&tree),
        Err(err) => {
            tracing::warn!(version, error = %err, "harness layer not loaded");
            None
        }
    }
}

/// The prompt layer of the active version (`prompt size`).
#[must_use]
pub fn active_layer(scope: &ProfileScope) -> Option<String> {
    let version = HarnessDir::of(scope.dir()).active_id().ok()?;
    layer(scope, &version)
}

/// The validation rules of the profile: its cap, installed skills,
/// registered toolsets and known secrets.
#[must_use]
pub fn rules(scope: &ProfileScope, user_home: Option<&std::path::Path>) -> Rules {
    let config = scope.config();
    let catalog = pmt_skills::eject::profile_catalog_at(
        &config.skills,
        scope.dir(),
        None,
        user_home,
        pmt_skills::bundled::Bundled::builtin(),
    );
    let mut toolsets: std::collections::BTreeSet<String> = pmt_tools::toolset::builtin()
        .iter()
        .map(|t| t.name.clone())
        .collect();
    toolsets.extend(
        pmt_tools::toolset::BUNDLED_PLATFORMS
            .iter()
            .map(|p| pmt_tools::toolset::bundle_name(p)),
    );
    Rules {
        always_on_cap: usize::try_from(config.harness.always_on_cap_chars).unwrap_or(usize::MAX),
        installed_skills: catalog.skills.iter().map(|s| s.name.clone()).collect(),
        toolsets,
        redactor: scope.secrets().redactor().unwrap_or_default(),
    }
}
