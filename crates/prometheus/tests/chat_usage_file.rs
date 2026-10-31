//! `chat --usage-file` and the flags that ignore profile config and rules
//! (spec: cli, Usage report file; Ignore profile config and rules).

mod common;

use std::path::Path;

use common::{command, mock_home, newest_session, ok, run, stderr};

fn report(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A home whose `ollama` provider points at a closed port, failing at
/// once.
fn unreachable_home() -> tempfile::TempDir {
    let home = mock_home();
    for (key, value) in [
        ("providers.ollama.base_url", "http://127.0.0.1:9/v1"),
        ("providers.ollama.max_retries", "0"),
        ("agent.auto_recovery_cycles", "0"),
        ("budgets.allow_unpriced", "true"),
    ] {
        ok(&run(home.path(), &["config", "set", key, value]));
    }
    home
}

/// Scenario: Usage written on failure.
#[test]
fn the_usage_file_is_written_when_the_provider_is_unreachable() {
    let home = unreachable_home();
    let file = home.path().join("u.json");
    let out = run(
        home.path(),
        &[
            "-z",
            "hi",
            "--provider",
            "ollama",
            "-m",
            "llama3",
            "--usage-file",
            file.to_str().unwrap(),
        ],
    );
    assert_eq!(out.status.code(), Some(69), "{}", stderr(&out));
    let report = report(&file);
    assert_eq!(report["exit_code"], 69);
    assert_eq!(report["output_tokens"], 0);
    assert_eq!(report["exit_reason"], "unavailable");
    assert_eq!(report["provider"], "ollama");
    assert_eq!(report["model"], "llama3");
    assert_eq!(report["turns"], 1);
    for key in [
        "session_id",
        "root_session_id",
        "billing_source",
        "input_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "reasoning_tokens",
        "cost",
        "cost_status",
    ] {
        assert!(report.get(key).is_some(), "{key} missing: {report}");
    }
}

#[test]
fn a_successful_run_reports_its_tokens_and_billing() {
    let home = mock_home();
    let file = home.path().join("u.json");
    let out = ok(&run(
        home.path(),
        &["--json", "-z", "hi", "--usage-file", file.to_str().unwrap()],
    ));
    assert!(out.contains("mock: hi"), "{out}");
    let report = report(&file);
    assert_eq!(report["exit_code"], 0);
    assert_eq!(report["exit_reason"], "completed");
    assert_eq!(report["session_id"], newest_session(home.path()));
    assert_eq!(report["billing_source"], "keyless:mock");
    assert!(report["output_tokens"].as_u64().unwrap() > 0, "{report}");
}

#[test]
fn an_unwritable_usage_file_exits_73() {
    let home = mock_home();
    let file = home.path().join("missing-dir/u.json");
    let out = run(
        home.path(),
        &["-z", "hi", "--usage-file", file.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(73), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("cannot write --usage-file"),
        "{}",
        stderr(&out)
    );
}

#[cfg(unix)]
#[test]
fn the_usage_file_is_written_on_sigint() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let home = mock_home();
    let script = home.path().join("slow.toml");
    std::fs::write(
        &script,
        "[[rule]]\nrepeat = true\nlatency_ms = 20000\ntext = \"slow\"\n",
    )
    .unwrap();
    let file = home.path().join("u.json");
    let mut child = command(home.path())
        .args(["-z", "hi", "--mock-script", script.to_str().unwrap()])
        .args(["--usage-file", file.to_str().unwrap()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(2));
    kill(
        Pid::from_raw(i32::try_from(child.id()).unwrap()),
        Signal::SIGINT,
    )
    .unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(130));
    let report = report(&file);
    assert_eq!(report["exit_code"], 130);
    assert_eq!(report["exit_reason"], "interrupted");
}

/// The system prompt the newest session started with (its stored tiers).
fn system_prompt(home: &Path) -> String {
    use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
    let id: pmt_core::id::SessionId = newest_session(home).parse().unwrap();
    let home = Home::open(home).unwrap();
    let profile = home
        .existing_profile(&ProfileName::parse("default").unwrap())
        .unwrap();
    let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
    let store = pmt_store::Store::open(&scope).unwrap();
    let prompt = store.session_prompt(id).unwrap().expect("a stored prompt");
    prompt
        .tiers
        .into_iter()
        .map(|t| t.text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Scenario: Rules skipped.
#[test]
fn ignore_rules_skips_rule_files_identity_and_memory_and_is_recorded() {
    let home = mock_home();
    std::fs::write(home.path().join("AGENTS.md"), "RULE-FROM-AGENTS-MD\n").unwrap();
    std::fs::write(
        home.path().join("profiles/default/SOUL.md"),
        "IDENTITY-FROM-SOUL\n",
    )
    .unwrap();
    ok(&run(home.path(), &["memory", "add", "MEMORY-FACT-XYZ"]));
    let mut cmd = command(home.path());
    ok(&cmd
        .current_dir(home.path())
        .args(["-z", "hi"])
        .output()
        .unwrap());
    let with = system_prompt(home.path());
    assert!(
        with.contains("RULE-FROM-AGENTS-MD") && with.contains("IDENTITY-FROM-SOUL"),
        "{with}"
    );
    assert!(with.contains("MEMORY-FACT-XYZ"), "{with}");
    let mut cmd = command(home.path());
    ok(&cmd
        .current_dir(home.path())
        .args(["--ignore-rules", "-z", "hi"])
        .output()
        .unwrap());
    let without = system_prompt(home.path());
    for absent in [
        "RULE-FROM-AGENTS-MD",
        "IDENTITY-FROM-SOUL",
        "MEMORY-FACT-XYZ",
    ] {
        assert!(!without.contains(absent), "{absent} in {without}");
    }
    let shown = ok(&run(
        home.path(),
        &["sessions", "show", &newest_session(home.path())],
    ));
    assert!(shown.contains("ignore_rules = true"), "{shown}");
}

#[test]
fn ignore_user_config_runs_on_schema_defaults_with_this_command_line() {
    let home = mock_home();
    std::fs::write(
        home.path().join("profiles/default/SOUL.md"),
        "IDENTITY-FROM-SOUL\n",
    )
    .unwrap();
    ok(&run(
        home.path(),
        &["config", "set", "prompt.load_identity", "false"],
    ));
    ok(&run(home.path(), &["-z", "hi"]));
    assert!(!system_prompt(home.path()).contains("IDENTITY-FROM-SOUL"));
    let out = run(home.path(), &["--ignore-user-config", "-z", "hi"]);
    assert_eq!(
        out.status.code(),
        Some(78),
        "the profile's model is ignored too"
    );
    ok(&run(
        home.path(),
        &["--ignore-user-config", "--provider", "mock", "-z", "hi"],
    ));
    assert!(system_prompt(home.path()).contains("IDENTITY-FROM-SOUL"));
    let id = newest_session(home.path());
    let shown = ok(&run(home.path(), &["--json", "sessions", "show", &id]));
    let shown: serde_json::Value = serde_json::from_str(&shown).unwrap();
    assert_eq!(shown["session"]["ignore_user_config"], true);
    assert_eq!(shown["session"]["ignore_rules"], false);
}
