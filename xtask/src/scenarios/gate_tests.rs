//! The gate against a small repository: one spec, one test file, and an
//! index that is complete, then broken in each way the gate must catch.

use std::path::{Path, PathBuf};

use super::index::Index;
use super::{check, spec};

const SPEC: &str = "\
# demo

### Requirement: Greeting
The system SHALL greet.
**Milestone:** M1

#### Scenario: Hello
- **WHEN** asked
- **THEN** it greets

#### Scenario: Goodbye
- **WHEN** leaving
- **THEN** it says goodbye

### Requirement: Later
The system SHALL do more later.
**Milestone:** M2

#### Scenario: Not yet
- **WHEN** later
- **THEN** more
";

const TESTS: &str = "\
#[test]
fn greets() {}

fn helper() {}
";

const HEADER: &str = "specs = \"specs\"\nmilestone = \"M1\"\ncapabilities = [\"demo\"]\n\n";

fn repo(name: &str) -> PathBuf {
    let root = crate::repo_root()
        .join("target/xtask-test/scenarios")
        .join(name);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("specs/demo")).unwrap();
    std::fs::write(root.join("specs/demo/spec.md"), SPEC).unwrap();
    std::fs::write(root.join("tests.rs"), TESTS).unwrap();
    root
}

fn problems(root: &Path, entries: &str) -> Vec<String> {
    let index = Index::parse(&format!("{HEADER}{entries}")).unwrap();
    let scenarios = spec::collect(
        &root.join(&index.specs),
        &index.capabilities,
        &index.milestone,
    )
    .unwrap();
    check(root, &index, &scenarios)
}

#[test]
fn only_the_milestone_scenarios_are_collected() {
    let names: Vec<String> = spec::parse("demo", SPEC, "M1")
        .into_iter()
        .map(|s| s.key())
        .collect();
    assert_eq!(
        names,
        ["demo › Greeting › Hello", "demo › Greeting › Goodbye"]
    );
}

#[test]
fn a_complete_index_passes_and_counts_deferred_entries() {
    let root = repo("complete");
    let entries = "[demo.Greeting]\n\
        Hello = [\"tests.rs::greets\"]\n\
        Goodbye = { deferred = \"M2: needs the farewell module\" }\n";
    assert_eq!(problems(&root, entries), Vec::<String>::new());
    let index = Index::parse(&format!("{HEADER}{entries}")).unwrap();
    assert_eq!(index.deferred().len(), 1);
}

#[test]
fn every_way_an_index_drifts_is_reported() {
    let root = repo("drift");
    let entries = "[demo.Greeting]\n\
        Hello = { tests = [\"tests.rs::helper\", \"tests.rs::gone\", \"missing.rs::x\"], note = \"n\" }\n\
        Removed = [\"tests.rs::greets\"]\n\
        [demo.Later]\n\
        \"Not yet\" = { deferred = \" \" }\n";
    let found = problems(&root, entries);
    let expected = [
        "no entry for scenario demo › Greeting › Goodbye",
        "demo › Greeting › Hello: tests.rs::helper has no test attribute",
        "demo › Greeting › Hello: tests.rs has no fn gone",
        "demo › Greeting › Hello: missing.rs does not exist",
        "entry demo › Greeting › Removed names no M1 scenario of the specs",
        "entry demo › Later › Not yet names no M1 scenario of the specs",
        "demo › Later › Not yet: a deferred entry needs a reason",
    ];
    for problem in expected {
        assert!(
            found.iter().any(|p| p == problem),
            "{problem} not in {found:#?}"
        );
    }
    assert_eq!(found.len(), expected.len(), "{found:#?}");
}
