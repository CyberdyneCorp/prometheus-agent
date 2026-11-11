//! `prometheus migrate from-claude-code|from-codex|from-cursor|
//! from-openclaw`, `migrate detect`, `migrate cleanup openclaw` (spec:
//! hermes-migration, Importers for other agents; Other agents never yield
//! secrets by default; Claude Code, Codex CLI and Cursor mappings;
//! OpenClaw mapping; Source archiving after import), against fixture
//! homes of each agent in the test's user home.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use common::hermes::attributes;
use rusqlite::Connection;
use serde_json::Value;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn user(home: &Path) -> PathBuf {
    let dir = common::user_home(home);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn migrate(home: &Path, args: &[&str]) -> Output {
    common::command(home)
        .arg("--json")
        .arg("migrate")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn report(out: &Output, code: i32) -> Value {
    assert_eq!(
        out.status.code(),
        Some(code),
        "stdout {}\nstderr {}",
        common::stdout(out),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(common::stdout(out).trim()).unwrap()
}

fn entries(report: &Value) -> Vec<Value> {
    report["profiles"][0]["entries"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn entry(report: &Value, item: &str) -> Value {
    entries(report)
        .into_iter()
        .find(|e| e["item"] == item)
        .unwrap_or_else(|| panic!("no entry {item}: {:#?}", entries(report)))
}

fn profile(home: &Path) -> PathBuf {
    home.join("profiles/default")
}

fn memories(home: &Path) -> Vec<String> {
    let conn = Connection::open(profile(home).join("state.db")).unwrap();
    let mut stmt = conn
        .prepare("SELECT content FROM memory_entries ORDER BY created_at")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// Every file under `dir` containing `needle`.
fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for (path, (bytes, ..)) in attributes(dir) {
        if String::from_utf8_lossy(&bytes).contains(needle) {
            out.push(path);
        }
    }
    out
}

fn claude_home(home: &Path) -> PathBuf {
    let dir = user(home).join(".claude");
    write(
        &dir.join("CLAUDE.md"),
        "# Style\n\nUse tabs.\n\nNo emoji.\n",
    );
    write(
        &dir.join("skills/review/SKILL.md"),
        "---\nname: review\ndescription: Review a diff.\n---\nRead the diff carefully.\n",
    );
    write(&dir.join("commands/deploy.md"), "Deploy the app.\n");
    write(
        &dir.join(".credentials.json"),
        r#"{"claudeAiOauth": {"accessToken": "sk-ant-oat-secret"}}"#,
    );
    dir
}

#[test]
fn detect_lists_only_the_agents_present() {
    let tmp = common::temp_home();
    let home = tmp.path();
    claude_home(home);
    write(
        &user(home).join(".cursor/rules/rust.mdc"),
        "---\nalwaysApply: true\n---\nPrefer iterators.\n",
    );
    let out = common::run(home, &["--json", "migrate", "detect"]);
    let found: Value = serde_json::from_str(common::stdout(&out).trim()).unwrap();
    let agents: Vec<&str> = found["detected"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["agent"].as_str().unwrap())
        .collect();
    assert_eq!(agents, ["claude-code", "cursor"]);
    let claude = &found["detected"][0]["counts"];
    assert_eq!(claude["memory"], 1);
    assert_eq!(
        claude["skills"], 2,
        "a skill and an unsupported command: {claude}"
    );
    assert!(!home.join("profiles").exists(), "detect wrote to the home");
}

#[test]
fn a_broken_settings_file_fails_alone_and_the_rest_imports() {
    let tmp = common::temp_home();
    let home = tmp.path();
    let source = claude_home(home);
    write(&source.join("settings.json"), "{ not json");
    let before = attributes(&source);
    let r = report(&migrate(home, &["from-claude-code", "--yes"]), 1);
    let failed = entry(&r, "settings.json");
    assert_eq!(failed["disposition"], "failed");
    assert!(
        failed["reason"]
            .as_str()
            .unwrap()
            .contains("not valid JSON"),
        "{failed}"
    );
    assert!(memories(home).iter().any(|m| m.contains("Use tabs.")));
    assert_eq!(
        entry(&r, "commands/deploy.md")["disposition"],
        "unsupported"
    );
    assert!(
        profile(home)
            .join("skills/claude-code-imports/review/SKILL.md")
            .is_file()
    );
    assert_eq!(attributes(&source), before, "the source changed");
    assert!(
        files_containing(home, "sk-ant-oat-secret")
            .into_iter()
            .all(|p| p.starts_with(&source)),
        "a credential file was read"
    );
}

#[test]
fn a_prefix_rule_becomes_an_approval_and_other_rules_are_unsupported() {
    let tmp = common::temp_home();
    let home = tmp.path();
    let source = claude_home(home);
    write(
        &source.join("settings.json"),
        r#"{"permissions": {"allow": ["Bash(npm run test:*)", "WebFetch"], "deny": ["Bash(rm -rf:*)"]}}"#,
    );
    let r = report(
        &migrate(home, &["from-claude-code", "--yes", "--no-backup"]),
        0,
    );
    let grants = std::fs::read_to_string(profile(home).join("grants.toml")).unwrap();
    assert!(grants.contains("\"npm run test*\""), "{grants}");
    assert_eq!(
        entry(&r, "settings.json:permissions.allow:WebFetch")["disposition"],
        "unsupported"
    );
    let config = std::fs::read_to_string(profile(home).join("config.toml")).unwrap();
    assert!(config.contains("rm -rf*"), "{config}");
}

#[test]
fn an_mcp_token_is_stripped_and_named() {
    let tmp = common::temp_home();
    let home = tmp.path();
    let source = user(home).join(".cursor");
    write(
        &source.join("mcp.json"),
        r#"{"mcpServers": {"github": {"command": "npx", "args": ["-y", "gh-mcp"], "env": {"GITHUB_TOKEN": "ghp_x", "LOG_LEVEL": "info"}}}}"#,
    );
    let r = report(&migrate(home, &["from-cursor", "--yes"]), 0);
    let secret = entry(&r, "mcp.json:mcpServers.github:GITHUB_TOKEN");
    assert_eq!(secret["disposition"], "secret");
    assert!(
        secret["reason"]
            .as_str()
            .unwrap()
            .contains("prometheus secrets set GITHUB_TOKEN")
    );
    let config = std::fs::read_to_string(profile(home).join("config.toml")).unwrap();
    assert!(
        config.contains("env_secrets = [\"GITHUB_TOKEN\"]"),
        "{config}"
    );
    assert!(files_containing(&profile(home), "ghp_x").is_empty());
    assert!(!common::stdout(&migrate(home, &["from-cursor", "--dry-run"])).contains("ghp_x"));
}

#[test]
fn memory_already_held_is_not_added_twice() {
    let tmp = common::temp_home();
    let home = tmp.path();
    claude_home(home);
    write(
        &user(home).join(".codex/AGENTS.md"),
        "No emoji.\n\nRun the tests first.\n",
    );
    write(
        &user(home).join(".codex/auth.json"),
        r#"{"OPENAI_API_KEY": "sk-codex"}"#,
    );
    report(&migrate(home, &["from-claude-code", "--yes"]), 0);
    report(&migrate(home, &["from-codex", "--yes"]), 0);
    let all = memories(home);
    assert_eq!(
        all.iter().filter(|m| m.as_str() == "No emoji.").count(),
        1,
        "{all:?}"
    );
    assert!(all.iter().any(|m| m == "Run the tests first."), "{all:?}");
    assert!(files_containing(&profile(home), "sk-codex").is_empty());
}

#[test]
fn a_real_run_snapshots_the_profile_first_unless_no_backup() {
    let tmp = common::temp_home();
    let home = tmp.path();
    claude_home(home);
    common::run(home, &["profile", "list"]);
    report(&migrate(home, &["from-claude-code", "--yes"]), 0);
    let list = common::run(home, &["--json", "snapshots", "list"]);
    assert!(
        common::stdout(&list).contains("pre-migration"),
        "{}",
        common::stdout(&list)
    );
}

#[test]
fn an_importer_needs_yes_without_a_terminal() {
    let tmp = common::temp_home();
    let home = tmp.path();
    claude_home(home);
    let out = migrate(home, &["from-claude-code"]);
    assert_eq!(out.status.code(), Some(4));
    assert!(!profile(home).join("skills/claude-code-imports").exists());
}

fn openclaw_home(home: &Path) -> PathBuf {
    let dir = user(home).join(".openclaw");
    write(
        &dir.join("openclaw.json"),
        r#"{
          // JSON5-style comments are accepted
          "agents": {"defaults": {
            "model": {"primary": "anthropic/claude-sonnet-4", "fallbacks": ["openai/gpt-5"]},
            "thinkingDefault": "adaptive",
            "timeoutSeconds": 600,
            "userTimezone": "Europe/Lisbon",
          }},
          "models": {"providers": {"acme": {"baseUrl": "https://llm.acme.test/v1", "apiKey": "acme-secret-1", "api": "openai-completions"}}},
          "session": {"idleMinutes": 60},
        }"#,
    );
    write(&dir.join("workspace/SOUL.md"), "You are Molty.\n");
    write(
        &dir.join("workspace/MEMORY.md"),
        "likes rust\n§\nlives in Lisbon\n",
    );
    write(
        &dir.join("workspace/skills/notes/SKILL.md"),
        "---\nname: notes\ndescription: Notes.\n---\nTake notes.\n",
    );
    dir
}

#[test]
fn openclaw_full_maps_reasoning_effort_and_keeps_secrets_out() {
    let tmp = common::temp_home();
    let home = tmp.path();
    let source = openclaw_home(home);
    let before = attributes(&source);
    let r = report(
        &migrate(home, &["from-openclaw", "--preset", "full", "--yes"]),
        0,
    );
    let thinking = entry(&r, "agents.defaults.thinkingDefault");
    assert_eq!(thinking["disposition"], "transformed");
    let config = std::fs::read_to_string(profile(home).join("config.toml")).unwrap();
    assert!(config.contains("reasoning_effort = \"medium\""), "{config}");
    assert!(config.contains("max_iterations = 60"), "{config}");
    assert!(config.contains("timezone = \"Europe/Lisbon\""), "{config}");
    let key = entry(&r, "models.providers.acme.apiKey");
    assert_eq!(key["disposition"], "secret");
    assert!(
        key["reason"]
            .as_str()
            .unwrap()
            .contains("--migrate-secrets")
    );
    assert!(files_containing(&profile(home), "acme-secret-1").is_empty());
    assert_eq!(
        entry(&r, "session.idleMinutes")["disposition"],
        "unsupported"
    );
    let soul = std::fs::read_to_string(profile(home).join("SOUL.md")).unwrap();
    assert_eq!(soul, "You are Molty.\n");
    assert!(memories(home).iter().any(|m| m == "lives in Lisbon"));
    assert!(
        profile(home)
            .join("skills/openclaw-imports/notes/SKILL.md")
            .is_file()
    );
    // Cleanup is explicit: the import left the source as it was.
    assert_eq!(attributes(&source), before);

    let r = report(
        &migrate(
            home,
            &[
                "from-openclaw",
                "--preset",
                "full",
                "--migrate-secrets",
                "--yes",
            ],
        ),
        0,
    );
    assert_eq!(
        entry(&r, "models.providers.acme.apiKey")["disposition"],
        "secret"
    );
    let secrets = std::fs::read_to_string(profile(home).join("secrets.toml")).unwrap();
    assert!(secrets.contains("ACME_API_KEY"), "{secrets}");
    let config = std::fs::read_to_string(profile(home).join("config.toml")).unwrap();
    assert!(!config.contains("acme-secret-1"));
}

#[test]
fn openclaw_user_data_leaves_infrastructure_out() {
    let tmp = common::temp_home();
    let home = tmp.path();
    openclaw_home(home);
    let r = report(&migrate(home, &["from-openclaw", "--yes"]), 0);
    assert_eq!(
        entry(&r, "agents.defaults.thinkingDefault")["disposition"],
        "skipped"
    );
    let config = std::fs::read_to_string(profile(home).join("config.toml")).unwrap();
    assert!(!config.contains("reasoning_effort"), "{config}");
    assert!(config.contains("Europe/Lisbon"), "{config}");
}

#[test]
fn cleanup_archives_openclaw_only_on_request() {
    let tmp = common::temp_home();
    let home = tmp.path();
    let source = openclaw_home(home);
    let out = migrate(home, &["cleanup", "openclaw"]);
    assert_eq!(
        out.status.code(),
        Some(4),
        "cleanup without --yes and a terminal"
    );
    assert!(source.is_dir());
    let out = migrate(home, &["cleanup", "openclaw", "--yes"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!source.exists());
    assert!(
        user(home)
            .join(".openclaw.pre-migration/openclaw.json")
            .is_file()
    );
}

#[test]
fn cleanup_refuses_while_openclaw_runs() {
    let tmp = common::temp_home();
    let home = tmp.path();
    let source = openclaw_home(home);
    write(&source.join("gateway.pid"), &std::process::id().to_string());
    let out = migrate(home, &["cleanup", "openclaw", "--yes"]);
    assert_eq!(
        out.status.code(),
        Some(75),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(source.join("openclaw.json").is_file());
}
