//! `cargo xtask scenarios`: the scenario-to-test index gate (task 21.3).
//!
//! `scenario-index.toml` maps every scenario of the milestone's
//! requirements, for the listed capabilities, to the tests that prove it.
//! The gate fails when a scenario of the specs has no entry, when an entry
//! names a scenario the specs no longer have, or when a named test does
//! not exist as a test function (a `#[test]`-style attribute directly
//! above `fn <name>`, not `#[ignore]`d). `scripts/check` runs it next to
//! the test suite, which proves the named tests pass.
//!
//! An entry may instead be `{ deferred = "<why>" }` for a scenario whose
//! subject belongs to a later milestone; the gate lists those on every run
//! so they stay visible.

mod index;
mod spec;
mod tests_in_source;

use std::collections::BTreeSet;
use std::path::Path;

use self::index::{Entry, Index};
use self::spec::Scenario;

/// The index, relative to the repository root.
pub(crate) const INDEX: &str = "scenario-index.toml";

/// Checks the index of `root`.
pub(crate) fn run(root: &Path) -> Result<(), String> {
    let index = Index::load(&root.join(INDEX))?;
    let scenarios = spec::collect(
        &root.join(&index.specs),
        &index.capabilities,
        &index.milestone,
    )?;
    let problems = check(root, &index, &scenarios);
    let deferred = index.deferred();
    for (key, reason) in &deferred {
        println!("scenarios: deferred: {key}: {reason}");
    }
    if problems.is_empty() {
        println!(
            "scenarios: {} {} scenarios of {} capabilities indexed; {} mapped to tests, {} deferred",
            scenarios.len(),
            index.milestone,
            index.capabilities.len(),
            scenarios.len() - deferred.len(),
            deferred.len()
        );
        return Ok(());
    }
    for problem in &problems {
        eprintln!("scenarios: {problem}");
    }
    Err(format!("{} problem(s) in {INDEX}", problems.len()))
}

/// Every mismatch between the specs, the index and the test sources.
fn check(root: &Path, index: &Index, scenarios: &[Scenario]) -> Vec<String> {
    let expected: BTreeSet<String> = scenarios.iter().map(Scenario::key).collect();
    let mut problems = Vec::new();
    for key in &expected {
        if !index.entries.contains_key(key) {
            problems.push(format!("no entry for scenario {key}"));
        }
    }
    let mut sources = tests_in_source::Sources::new(root);
    for (key, entry) in &index.entries {
        if !expected.contains(key) {
            problems.push(format!(
                "entry {key} names no {} scenario of the specs",
                index.milestone
            ));
        }
        problems.extend(check_entry(key, entry, &mut sources));
    }
    problems
}

fn check_entry(key: &str, entry: &Entry, sources: &mut tests_in_source::Sources) -> Vec<String> {
    match entry {
        Entry::Deferred(reason) if reason.trim().is_empty() => {
            vec![format!("{key}: a deferred entry needs a reason")]
        }
        Entry::Deferred(_) => Vec::new(),
        Entry::Tests(tests) if tests.is_empty() => vec![format!("{key}: names no test")],
        Entry::Tests(tests) => tests
            .iter()
            .filter_map(|test| sources.check(test).err())
            .map(|err| format!("{key}: {err}"))
            .collect(),
    }
}

#[cfg(test)]
mod gate_tests;
