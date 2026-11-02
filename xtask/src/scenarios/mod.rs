//! `cargo xtask scenarios`: the scenario-to-test index gate (task 21.3).
//!
//! `scenario-index.toml` maps the scenarios of each indexed milestone's
//! requirements, for that milestone's capabilities, to the tests that
//! prove them. The gate fails when a scenario of a complete milestone has
//! no entry, when an entry names a scenario no indexed milestone has, or
//! when a named test does not exist as a test function (a `#[test]`-style
//! attribute directly above `fn <name>`, not `#[ignore]`d). A milestone
//! marked `partial` is being implemented: its unindexed scenarios are
//! counted as pending, never failures. `scripts/check` runs the gate next
//! to the test suite, which proves the named tests pass.
//!
//! An entry may instead be `{ deferred = "<why>" }` for a scenario whose
//! subject belongs to a later milestone; the gate lists those on every run
//! so they stay visible.

mod index;
mod spec;
mod tests_in_source;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use self::index::{Entry, Index};
use self::spec::Scenario;

/// The index, relative to the repository root.
pub(crate) const INDEX: &str = "scenario-index.toml";

/// Checks the index of `root`.
pub(crate) fn run(root: &Path) -> Result<(), String> {
    let index = Index::load(&root.join(INDEX))?;
    let scenarios = collect(root, &index)?;
    let problems = check(root, &index, &scenarios);
    for (key, reason) in index.deferred() {
        println!("scenarios: deferred: {key}: {reason}");
    }
    if problems.is_empty() {
        for line in summary(&index, &scenarios) {
            println!("scenarios: {line}");
        }
        return Ok(());
    }
    for problem in &problems {
        eprintln!("scenarios: {problem}");
    }
    Err(format!("{} problem(s) in {INDEX}", problems.len()))
}

/// The scenarios of every indexed milestone.
fn collect(root: &Path, index: &Index) -> Result<Vec<Scenario>, String> {
    let mut all = Vec::new();
    for milestone in &index.milestones {
        all.extend(spec::collect(
            &root.join(&index.specs),
            &milestone.capabilities,
            &milestone.name,
        )?);
    }
    Ok(all)
}

/// One line per milestone: scenarios, mapped, deferred and pending.
fn summary(index: &Index, scenarios: &[Scenario]) -> Vec<String> {
    index
        .milestones
        .iter()
        .map(|milestone| {
            let mut counts = BTreeMap::<&str, usize>::new();
            let own = scenarios.iter().filter(|s| s.milestone == milestone.name);
            for scenario in own {
                let state = match index.entries.get(&scenario.key()) {
                    Some(Entry::Tests(_)) => "mapped",
                    Some(Entry::Deferred(_)) => "deferred",
                    None => "pending",
                };
                *counts.entry("total").or_default() += 1;
                *counts.entry(state).or_default() += 1;
            }
            let count = |state| counts.get(state).copied().unwrap_or(0);
            format!(
                "{}{}: {} scenarios of {} capabilities; {} mapped to tests, {} deferred, {} pending",
                milestone.name,
                if milestone.partial { " (partial)" } else { "" },
                count("total"),
                milestone.capabilities.len(),
                count("mapped"),
                count("deferred"),
                count("pending"),
            )
        })
        .collect()
}

/// Every mismatch between the specs, the index and the test sources.
fn check(root: &Path, index: &Index, scenarios: &[Scenario]) -> Vec<String> {
    let partial: BTreeSet<&str> = index
        .milestones
        .iter()
        .filter(|m| m.partial)
        .map(|m| m.name.as_str())
        .collect();
    let expected: BTreeSet<String> = scenarios.iter().map(Scenario::key).collect();
    let mut problems = Vec::new();
    for scenario in scenarios {
        let key = scenario.key();
        if !partial.contains(scenario.milestone.as_str()) && !index.entries.contains_key(&key) {
            problems.push(format!("no entry for scenario {key}"));
        }
    }
    let names: Vec<&str> = index.milestones.iter().map(|m| m.name.as_str()).collect();
    let mut sources = tests_in_source::Sources::new(root);
    for (key, entry) in &index.entries {
        if !expected.contains(key) {
            problems.push(format!(
                "entry {key} names no {} scenario of the specs",
                names.join("/")
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
