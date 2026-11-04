//! The tamper-evident security audit log (spec: security-boundaries,
//! Tamper-evident security audit log): approval decisions land in
//! `<profile>/logs/security-audit.jsonl`, and `prometheus security audit
//! verify` names the first record edited by hand.

mod common;

use std::path::Path;

use common::{mock_home, ok, run, stderr, stdout};

/// A one-shot run whose model asks for a dangerous command; the run cannot
/// ask anyone, so the gate denies it and records the decision.
fn denied_command(home: &Path, command: &str) {
    let script = home.join("dangerous.toml");
    std::fs::write(
        &script,
        format!(
            "[[rule]]\nuser = \"clean\"\n[[rule.tool_calls]]\nname = \"terminal\"\narguments = {{ command = \"{command}\" }}\n\n[[rule]]\ntool_result = \"BLOCKED\"\ntext = \"could not\"\n"
        ),
    )
    .unwrap();
    let out = common::command(home)
        .current_dir(home)
        .args([
            "--in-process",
            "--mock-script",
            script.to_str().unwrap(),
            "-z",
            "clean",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
}

fn audit_path(home: &Path) -> std::path::PathBuf {
    home.join("profiles/default/logs/security-audit.jsonl")
}

/// Scenario: Edited record.
#[test]
fn editing_an_audit_record_by_hand_fails_verification_naming_its_line() {
    let home = mock_home();
    let home = home.path();
    denied_command(home, "rm -rf build");
    denied_command(home, "rm -rf dist");
    let text = std::fs::read_to_string(audit_path(home)).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    for line in &lines {
        let record: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(record["kind"], "approval", "{line}");
        assert_eq!(record["decision"], "deny", "{line}");
        assert!(
            record["session"]
                .as_str()
                .is_some_and(|s| s.starts_with("ses_")),
            "{line}"
        );
    }
    let verified = ok(&run(home, &["security", "audit", "verify"]));
    assert!(verified.contains("intact: 2 records"), "{verified}");

    std::fs::write(audit_path(home), text.replacen("rm -rf build", "ls", 1)).unwrap();
    let out = run(home, &["security", "audit", "verify"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("broken at line 1"),
        "{}",
        stdout(&out)
    );

    let json = run(home, &["--json", "security", "audit", "verify"]);
    assert_eq!(json.status.code(), Some(1));
    let body = stdout(&json);
    let report: serde_json::Value =
        serde_json::from_str(body.lines().last().unwrap_or_default()).unwrap();
    assert_eq!(report["broken_line"], 1, "{body}");
}
