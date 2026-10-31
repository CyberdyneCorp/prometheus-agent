//! `SECURITY.md` declares the trust model the security spec requires
//! (spec: security-boundaries, Declared trust model; In-scope findings and
//! private reporting; task 8.1): it names every security boundary, every
//! heuristic, every in-scope finding class, the private channel and the
//! disclosure window.

use std::path::Path;

fn policy() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../SECURITY.md");
    std::fs::read_to_string(path).unwrap()
}

/// The text between `heading` and the next heading of the same level.
fn section<'a>(text: &'a str, heading: &str) -> &'a str {
    let start = text
        .find(heading)
        .unwrap_or_else(|| panic!("missing {heading}"));
    let level = heading.split(' ').next().unwrap_or_default();
    let body = &text[start + heading.len()..];
    let end = body
        .match_indices(&format!("\n{level} "))
        .next()
        .map_or(body.len(), |(i, _)| i);
    &body[..end]
}

/// Checks each item appears, with line breaks read as spaces.
fn assert_names(section_text: &str, what: &str, items: &[&str]) {
    let text = section_text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for item in items {
        assert!(
            text.contains(item),
            "SECURITY.md {what} does not name '{item}'"
        );
    }
}

#[test]
fn every_spec_boundary_and_heuristic_is_classified() {
    let policy = policy();
    assert_names(
        section(&policy, "### Security boundaries"),
        "boundaries",
        &[
            "OS sandbox around local child processes",
            "Container and remote backends",
            "WASM plugin instances",
            "Out-of-process sidecars",
            "Python bridge",
            "MCP stdio servers",
            "Authenticated external surfaces",
            "prometheus doctor security",
        ],
    );
    assert_names(
        section(&policy, "### Heuristics"),
        "heuristics",
        &[
            "approval gate",
            "Output redaction",
            "Skill scanning",
            "Provenance markers",
            "Taint tracking",
        ],
    );
}

#[test]
fn in_scope_findings_and_the_private_channel_are_declared() {
    let policy = policy();
    assert_names(
        section(&policy, "## In scope (report privately)"),
        "in-scope findings",
        &[
            "Sandbox escape",
            "container or WASM",
            "capability that was not granted",
            "Surface authorization bypass",
            "Credential exfiltration",
            "Signature bypass",
            "plugins or",
            "releases",
            "hardening downgrade",
        ],
    );
    assert_names(
        section(&policy, "## Reporting a vulnerability"),
        "reporting",
        &["privately", "private vulnerability reporting", "90-day"],
    );
}
