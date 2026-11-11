//! `prometheus migrate sync` and `prometheus migrate sessions` (spec:
//! hermes-migration, Keeping imports in sync; Session transcripts from
//! other agent CLIs), against fixture Claude Code and Codex homes.

mod common;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use rusqlite::Connection;
use serde_json::{Value, json};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
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

fn ok(out: &Output) -> Value {
    assert!(
        out.status.success(),
        "exit {:?}\nstdout {}\nstderr {}",
        out.status.code(),
        common::stdout(out),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(common::stdout(out).trim()).unwrap()
}

fn skill(dir: &Path, name: &str) {
    write(
        &dir.join(name).join("SKILL.md"),
        &format!("---\nname: {name}\ndescription: The {name} skill.\n---\nDo {name}.\n"),
    );
}

fn claude(home: &Path) -> PathBuf {
    let dir = common::user_home(home).join(".claude");
    write(&dir.join("CLAUDE.md"), "Use tabs.\n");
    skill(&dir.join("skills"), "review");
    write(&dir.join(".credentials.json"), r#"{"accessToken": "one"}"#);
    dir
}

/// The one import a sync reported.
fn synced(out: &Output) -> Value {
    ok(out)["imports"][0].clone()
}

#[test]
fn a_new_skill_is_pulled_in_and_unchanged_sources_are_skipped() {
    let tmp = common::temp_home();
    let home = tmp.path();
    let source = claude(home);
    ok(&migrate(
        home,
        &["from-claude-code", "--yes", "--no-backup"],
    ));
    let registry =
        std::fs::read_to_string(home.join("profiles/default/migrations/import-sync.toml")).unwrap();
    assert!(registry.contains("claude-code"), "{registry}");

    let first = synced(&migrate(home, &["sync"]));
    assert_eq!(first["status"], "skipped: unchanged");

    skill(&source.join("skills"), "deploy");
    let second = synced(&migrate(home, &["sync"]));
    assert_eq!(second["status"], "synced");
    let imported: Vec<&Value> = second["report"]["profiles"][0]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["disposition"] == "mapped")
        .collect();
    assert_eq!(imported.len(), 1, "{imported:#?}");
    assert_eq!(imported[0]["item"], "skills/deploy");
    assert!(
        home.join("profiles/default/skills/claude-code-imports/deploy/SKILL.md")
            .is_file()
    );
    assert_eq!(
        synced(&migrate(home, &["sync"]))["status"],
        "skipped: unchanged"
    );
}

#[test]
fn a_token_refresh_does_not_trigger_a_sync() {
    let tmp = common::temp_home();
    let home = tmp.path();
    let source = claude(home);
    ok(&migrate(
        home,
        &["from-claude-code", "--yes", "--no-backup"],
    ));
    write(
        &source.join(".credentials.json"),
        r#"{"accessToken": "two"}"#,
    );
    let result = synced(&migrate(home, &["sync"]));
    assert_eq!(result["status"], "skipped: unchanged");
    assert!(
        result.get("report").is_none(),
        "an importer ran: {result:#}"
    );
}

#[test]
fn a_skill_edited_in_prometheus_is_kept_by_a_sync() {
    let tmp = common::temp_home();
    let home = tmp.path();
    let source = claude(home);
    ok(&migrate(
        home,
        &["from-claude-code", "--yes", "--no-backup"],
    ));
    let copy = home.join("profiles/default/skills/claude-code-imports/review/SKILL.md");
    write(
        &copy,
        "---\nname: review\ndescription: Mine.\n---\nMy own steps.\n",
    );
    write(
        &source.join("skills/review/SKILL.md"),
        "---\nname: review\ndescription: Review.\n---\nNew upstream steps.\n",
    );
    let result = synced(&migrate(home, &["sync"]));
    let review = result["report"]["profiles"][0]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["item"] == "skills/review")
        .unwrap()
        .clone();
    assert_eq!(review["disposition"], "conflict", "{review:#}");
    assert!(
        std::fs::read_to_string(&copy)
            .unwrap()
            .contains("My own steps.")
    );
}

/// The memory entries of the default profile.
fn memories(home: &Path) -> Vec<(String, String)> {
    let db = Connection::open(home.join("profiles/default/state.db")).unwrap();
    let mut stmt = db
        .prepare("SELECT id, content FROM memory_entries ORDER BY created_at, id")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn approvals_and_memory_removed_in_prometheus_stay_removed_after_a_sync() {
    let tmp = common::temp_home();
    let home = tmp.path();
    let source = claude(home);
    write(&source.join("CLAUDE.md"), "Use tabs.\n\nPrefer rust.\n");
    write(
        &source.join("settings.json"),
        r#"{"permissions": {"allow": ["Bash(npm test)"], "deny": ["Bash(rm -rf:*)"]}}"#,
    );
    let state = common::user_home(home).join(".claude.json");
    write(&state, r#"{"numStartups": 1, "mcpServers": {}}"#);
    ok(&migrate(
        home,
        &["from-claude-code", "--yes", "--no-backup"],
    ));
    let profile = home.join("profiles/default");
    let grants = std::fs::read_to_string(profile.join("grants.toml")).unwrap();
    assert!(grants.contains("npm test"), "{grants}");
    let config = std::fs::read_to_string(profile.join("config.toml")).unwrap();
    assert!(config.contains("rm -rf*"), "{config}");

    // Claude Code rewrites its state file on every launch: no sync.
    write(&state, r#"{"numStartups": 2, "mcpServers": {}}"#);
    assert_eq!(
        synced(&migrate(home, &["sync"]))["status"],
        "skipped: unchanged"
    );

    // The user drops the approval, the deny rule and one memory entry.
    std::fs::write(
        profile.join("grants.toml"),
        grants.replace("\"npm test\"", ""),
    )
    .unwrap();
    std::fs::write(
        profile.join("config.toml"),
        config.replace("\"rm -rf*\"", ""),
    )
    .unwrap();
    let (rust, _) = memories(home)
        .into_iter()
        .find(|(_, c)| c == "Prefer rust.")
        .unwrap();
    ok(&common::run(home, &["--json", "memory", "rm", &rust]));

    write(
        &source.join("CLAUDE.md"),
        "Use tabs.\n\nPrefer rust.\n\nWrite tests first.\n",
    );
    let result = synced(&migrate(home, &["sync"]));
    assert_eq!(result["status"], "synced", "{result:#}");
    let contents: Vec<String> = memories(home).into_iter().map(|(_, c)| c).collect();
    assert_eq!(contents, ["Use tabs.", "Write tests first."], "{result:#}");
    let grants = std::fs::read_to_string(profile.join("grants.toml")).unwrap();
    assert!(!grants.contains("npm test"), "{grants}");
    let config = std::fs::read_to_string(profile.join("config.toml")).unwrap();
    assert!(!config.contains("rm -rf*"), "{config}");
}

fn session_count(home: &Path) -> i64 {
    Connection::open(home.join("profiles/default/state.db"))
        .unwrap()
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap()
}

fn lines(values: &[Value]) -> String {
    values
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_reimported_codex_rollout_returns_the_existing_session() {
    let tmp = common::temp_home();
    let home = tmp.path();
    let rollout = common::user_home(home)
        .join(".codex/sessions/2026/11/01/rollout-2026-11-01T10-00-00-abc.jsonl");
    write(
        &rollout,
        &lines(&[
            json!({"type": "session_meta", "payload": {"id": "019a7c2e-codex"}}),
            json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "rename the module"}]}}),
            json!({"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Renamed."}]}}),
        ]),
    );
    let file = rollout.to_str().unwrap();
    let first = ok(&migrate(home, &["sessions", "--from", "codex", file]));
    assert_eq!(first["created"], true);
    let second = ok(&migrate(home, &["sessions", "--from", "codex", file]));
    assert_eq!(second["created"], false);
    assert_eq!(second["session_id"], first["session_id"]);
    assert_eq!(session_count(home), 1);
    let id = first["session_id"].as_str().unwrap();
    let human = common::run(home, &["migrate", "sessions", "--from", "codex", file]);
    assert!(
        common::stdout(&human).contains(id),
        "{}",
        common::stdout(&human)
    );
    let shown = common::run(home, &["--json", "sessions", "show", id]);
    let title =
        &serde_json::from_str::<Value>(common::stdout(&shown).trim()).unwrap()["session"]["title"];
    assert_eq!(title, "Imported from Codex CLI: rename the module");
}

#[test]
fn a_claude_log_imports_as_a_clean_transcript() {
    let tmp = common::temp_home();
    let home = tmp.path();
    let log = common::user_home(home).join(".claude/projects/-work-app/5e1f.jsonl");
    let output = (0..5000).fold(String::new(), |mut out, i| {
        let _ = writeln!(out, "output line {i}");
        out
    });
    write(
        &log,
        &lines(&[
            json!({"type": "user", "sessionId": "5e1f", "message": {"role": "user", "content": "run the build"}}),
            json!({"type": "assistant", "sessionId": "5e1f", "message": {"role": "assistant", "content": [{"type": "text", "text": "Running it."}, {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "make"}}]}}),
            json!({"type": "user", "sessionId": "5e1f", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": output}]}}),
            json!({"type": "assistant", "sessionId": "5e1f", "message": {"role": "assistant", "content": [{"type": "text", "text": "The build passed."}]}}),
        ]),
    );
    let imported = ok(&migrate(
        home,
        &["sessions", "--from", "claude-code", log.to_str().unwrap()],
    ));
    let id = imported["session_id"].as_str().unwrap();
    let conn = Connection::open(home.join("profiles/default/state.db")).unwrap();
    let mut stmt = conn
        .prepare("SELECT role, content FROM messages WHERE session_id = ?1 ORDER BY seq")
        .unwrap();
    let rows: Vec<(String, String)> = stmt
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0], ("user".to_owned(), "run the build".to_owned()));
    assert!(rows[1].1.contains("[ran tool: Bash]"), "{}", rows[1].1);
    assert!(rows[1].1.contains("The build passed."));
    assert!(!rows[1].1.contains("output line"), "tool output leaked");
}

#[test]
fn the_picker_needs_a_terminal_without_a_file() {
    let tmp = common::temp_home();
    let home = tmp.path();
    let out = migrate(home, &["sessions", "--from", "codex"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
