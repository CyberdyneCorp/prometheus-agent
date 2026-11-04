//! The website blocklist (spec: security-boundaries, Website blocklist):
//! hosts named by `security.egress.deny` or a shared list file are refused
//! by the web tools and the children's egress proxy in every mode, each
//! refusal recorded as `egress.denied` with reason `denylist`.

#![cfg(unix)]

mod common;

use std::path::Path;

use common::{mock_home, ok, run, run_with_stdin, stderr};
use serde_json::Value;

/// A one-shot chat whose model makes `calls` (TOML tool call tables).
fn one_shot(home: &Path, calls: &str) {
    let script = home.join("blocklist.toml");
    std::fs::write(
        &script,
        format!("[[rule]]\nstep = 0\n{calls}\n[[rule]]\nstep = 1\ntext = \"done\"\n"),
    )
    .unwrap();
    let out = common::command(home)
        .current_dir(home)
        .args(["chat", "--in-process", "--mock-script"])
        .arg(&script)
        .args(["-z", "fetch"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
}

fn tool_output(home: &Path) -> String {
    common::active_messages(home)
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["content"].as_str().map(str::to_owned))
        .collect()
}

fn denials(home: &Path) -> Vec<Value> {
    let session = common::newest_session(home);
    ok(&run(
        home,
        &["trajectory", "show", &session, "--format", "jsonl"],
    ))
    .lines()
    .map(|line| serde_json::from_str::<Value>(line).unwrap())
    .filter(|event| event["type"] == "egress.denied")
    .map(|event| event["payload"].clone())
    .collect()
}

/// A home whose blocklist file names `*.casino.example`, with a web
/// extraction backend that is never reached.
fn blocklisted_home() -> tempfile::TempDir {
    let home = mock_home();
    let list = home.path().join("blocked.txt");
    std::fs::write(&list, "# shared by the team\n*.casino.example\n").unwrap();
    common::append_config(
        home.path(),
        &format!(
            "\n[security.egress]\ndeny = [\"tracker.example\"]\ndeny_files = [\"{}\"]\n",
            list.display()
        ),
    );
    ok(&run(
        home.path(),
        &["config", "set", "web.backend", "firecrawl"],
    ));
    ok(&run(
        home.path(),
        &[
            "config",
            "set",
            "web.firecrawl.base_url",
            "http://127.0.0.1:9",
        ],
    ));
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "FIRECRAWL_API_KEY"],
        "fc-test-key\n",
    ));
    home
}

/// Scenario: Shared list file.
#[test]
fn web_extract_of_a_host_in_a_shared_list_file_is_blocked() {
    let home = blocklisted_home();
    let home = home.path();
    one_shot(
        home,
        "[[rule.tool_calls]]\nname = \"web_extract\"\narguments = { urls = [\"https://play.casino.example/\"] }\n",
    );
    let output = tool_output(home);
    assert!(
        output.contains("Blocked: play.casino.example is blocked by policy"),
        "{output}"
    );
    let denied = denials(home);
    assert!(
        denied
            .iter()
            .any(|d| d["host"] == "play.casino.example" && d["reason"] == "denylist"),
        "{denied:?}"
    );
}

/// The children's proxy refuses blocklisted hosts too, configured or from
/// the file, in public mode.
#[test]
fn a_sandboxed_child_cannot_reach_a_blocklisted_host() {
    let home = blocklisted_home();
    let home = home.path();
    one_shot(
        home,
        "[[rule.tool_calls]]\nname = \"terminal\"\narguments = { command = 'curl -sS http://play.casino.example/; echo; curl -sS http://tracker.example/' }\n",
    );
    let output = tool_output(home);
    assert!(
        output.contains("Blocked: play.casino.example is blocked by policy"),
        "{output}"
    );
    assert!(
        output.contains("Blocked: tracker.example is blocked by policy"),
        "{output}"
    );
    let denied = denials(home);
    let reasons: Vec<&Value> = denied.iter().map(|d| &d["reason"]).collect();
    assert!(
        denied.len() >= 2 && reasons.iter().all(|r| *r == "denylist"),
        "{denied:?}"
    );
}
