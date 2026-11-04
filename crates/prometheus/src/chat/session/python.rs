//! Python skill scripts in a session's terminal (spec: python-bridge,
//! Python skill scripts; skills, Skill scripts run sandboxed with managed
//! runtimes): a command that runs a file of a skill directory gets that
//! skill's pinned environment (or, for a skill without a `python`
//! declaration, the managed interpreter with no extra packages) first on
//! `PATH`, so `python`, `python3` and `pip` resolve there. It still runs
//! under the session's sandbox, which may read the environments and the
//! managed runtime; plugin environments are never on that path.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use pmt_config::ProfileScope;
use pmt_core::capability::{Capability, CapabilitySet};
use pmt_pybridge::skills::{self, SkillPython};
use pmt_skills::session::SkillsSession;
use pmt_skills::source::SkillDir;
use pmt_tools::terminal::PathSource;

use crate::plugins_host::bridge::prepare::toolchain;

/// Reads the session sandbox needs for skill environments: the profile's
/// skill environments, the managed interpreter and uv.
#[must_use]
pub fn sandbox_reads(scope: &ProfileScope) -> CapabilitySet {
    let runtimes = scope.home().join("runtimes");
    let mut dirs = vec![
        skills::root(scope.dir()),
        runtimes.join("python"),
        runtimes.join("uv"),
    ];
    let resolved: Vec<PathBuf> = dirs
        .iter()
        .filter_map(|d| std::fs::canonicalize(d).ok())
        .collect();
    dirs.extend(resolved);
    dirs.iter().map(|d| Capability::read_under(d)).collect()
}

/// The `PATH` source of a session's terminal.
#[must_use]
pub fn path_source(scope: &ProfileScope, skills: &Arc<SkillsSession>) -> PathSource {
    let session = Arc::downgrade(skills);
    let scope = scope.clone();
    let built: Arc<Mutex<HashMap<String, Option<PathBuf>>>> = Arc::default();
    Arc::new(move |command: &str| {
        let Some(session) = session.upgrade() else {
            return Vec::new();
        };
        let (catalog, config) = session.catalog();
        let mut dirs = Vec::new();
        for skill in session.visible(&catalog, &config) {
            let SkillDir::Disk(dir) = &skill.dir else {
                continue;
            };
            if !command.contains(&dir.display().to_string()) {
                continue;
            }
            let mut cache = built.lock().unwrap_or_else(PoisonError::into_inner);
            let bin = cache
                .entry(skill.name.clone())
                .or_insert_with(|| bin_for(&scope, &skill.name, &skill.front))
                .clone();
            dirs.extend(bin);
        }
        dirs
    })
}

/// The `bin` directory a skill's scripts run with.
fn bin_for(
    scope: &ProfileScope,
    name: &str,
    front: &serde_json::Map<String, serde_json::Value>,
) -> Option<PathBuf> {
    let (python, uv) = toolchain(scope)
        .inspect_err(|err| tracing::warn!(skill = %name, "{}", err.message()))
        .ok()?;
    let declared = pmt_skills::frontmatter::field(front, "python")
        .and_then(SkillPython::of)
        .unwrap_or_default();
    let allow = scope.config().python_bridge.allow_unbounded_requirements;
    match skills::ensure(
        &uv,
        &python,
        &skills::root(scope.dir()),
        name,
        &declared,
        allow,
    ) {
        Ok(env) => Some(env.bin),
        Err(err) => {
            tracing::warn!(skill = %name, "python environment unavailable: {}", err.message());
            None
        }
    }
}
