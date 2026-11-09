//! The dependency policy the gates enforce (spec: workspace-architecture,
//! Repository health gates; distribution-and-update, Reproducible, locked
//! builds). `cargo deny` itself runs in `scripts/check` and in the release
//! pipeline; these tests pin what it is given and where it runs, so a
//! change that widens the allowlist or moves the release gate after
//! publishing fails here.

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn read(path: &str) -> String {
    std::fs::read_to_string(root().join(path)).unwrap()
}

fn deny() -> toml::Table {
    read("deny.toml").parse().unwrap()
}

fn strings(value: &toml::Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_owned())
        .collect()
}

/// Scenario: Dependency with a disallowed license: the allowlist `cargo
/// deny check` (a `scripts/check` gate) enforces is exactly the spec's, so
/// a GPL-3.0 dependency fails it, and no per-crate exception admits GPL.
#[test]
fn a_gpl_dependency_is_outside_the_license_allowlist() {
    let deny = deny();
    let licenses = deny["licenses"].as_table().unwrap();
    let mut allowed = strings(&licenses["allow"]);
    allowed.sort();
    assert_eq!(
        allowed,
        [
            "Apache-2.0",
            "BSD-2-Clause",
            "BSD-3-Clause",
            "ISC",
            "MIT",
            "MPL-2.0",
            "Unicode-3.0",
            "Zlib"
        ]
    );
    for exception in licenses
        .get("exceptions")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
    {
        let extra = strings(&exception["allow"]);
        assert!(
            extra.iter().all(|license| !license.contains("GPL")),
            "{exception}"
        );
    }
    assert!(read("scripts/check").contains("step cargo deny check"));
}

/// Scenario: Unwaived advisory blocks the release: every advisory fails
/// `cargo deny` unless waived in `advisories.ignore`; the release's
/// finalize job runs that audit before it signs anything, and publishing
/// (the `host` job) needs the finalize job to succeed.
#[test]
fn an_unwaived_advisory_fails_the_release_before_anything_is_published() {
    let deny = deny();
    let advisories = deny["advisories"].as_table().unwrap();
    // A waiver names the advisory and why it does not reach a shipped
    // artifact; anything not waived fails `cargo deny`.
    for waiver in advisories["ignore"].as_array().unwrap() {
        let id = waiver.get("id").and_then(toml::Value::as_str).unwrap_or("");
        let reason = waiver
            .get("reason")
            .and_then(toml::Value::as_str)
            .unwrap_or("");
        assert!(
            id.starts_with("RUSTSEC-"),
            "waiver without an advisory id: {waiver}"
        );
        assert!(!reason.trim().is_empty(), "waiver {id} has no reason");
    }
    assert_eq!(advisories["yanked"].as_str(), Some("deny"));

    let gates = read("scripts/ci/release-dependency-gates");
    assert!(gates.contains("set -euo pipefail"));
    assert!(gates.contains("cargo deny --locked check"));

    let finalize = read(".github/workflows/release-finalize.yml");
    let at = |needle: &str| {
        finalize
            .find(needle)
            .unwrap_or_else(|| panic!("release-finalize.yml has no {needle:?}"))
    };
    assert!(at("run: scripts/ci/release-dependency-gates") < at("- name: Sign"));

    let release = read(".github/workflows/release.yml");
    let host = &release[release.find("\n  host:").unwrap()..];
    let host = &host[..host.find("\n  custom-release-channels:").unwrap()];
    assert!(host.contains("- custom-release-finalize"), "{host}");
    assert!(
        host.contains("needs.custom-release-finalize.result == 'success'"),
        "{host}"
    );
}
