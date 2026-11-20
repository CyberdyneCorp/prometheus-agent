//! Consolidation proposals, dormancy and waking through the binary (spec:
//! memory, Consolidation proposals; Decay, expiry and dormancy).

mod common;

use std::path::Path;

use common::{append_config, mock_home, ok, run, stderr};
use serde_json::{Value, json};

fn memory_json(home: &Path, args: &[&str]) -> Value {
    let mut full = vec!["--json", "memory"];
    full.extend_from_slice(args);
    serde_json::from_str(&ok(&run(home, &full))).unwrap()
}

fn db(home: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(home.join("profiles/default/state.db")).unwrap()
}

/// Adds a user entry last changed at `updated_ms`; its id.
fn add(home: &Path, content: &str, updated_ms: i64) -> String {
    ok(&run(home, &["memory", "add", content, "--target", "user"]));
    let id: String = db(home)
        .query_row(
            "SELECT id FROM memory_entries WHERE content = ?1",
            [content],
            |r| r.get(0),
        )
        .unwrap();
    db(home)
        .execute(
            "UPDATE memory_entries SET created_at = ?2, updated_at = ?2 WHERE id = ?1",
            rusqlite::params![id, updated_ms],
        )
        .unwrap();
    id
}

/// The auxiliary memory model answers `reply` to every call.
fn consolidator(home: &Path, reply: &Value) {
    std::fs::write(
        home.join("profiles/default/mock.toml"),
        format!("[[rule]]\nrepeat = true\ntext = {:?}\n", reply.to_string()),
    )
    .unwrap();
    append_config(
        home,
        "\n[providers.mock]\nscript = \"mock.toml\"\n\n[auxiliary.memory]\nprovider = \"mock\"\nmodel = \"memory-consolidator\"\n",
    );
}

fn contents(home: &Path) -> Vec<String> {
    memory_json(home, &["list", "--all"])
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["content"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn a_contradiction_is_proposed_as_a_diff_and_applied_only_on_request() {
    // Scenario: Contradiction is proposed, not silently applied.
    let home = mock_home();
    let vim = add(home.path(), "Uses vim.", 1_740_000_000_000);
    let helix = add(home.path(), "Switched to Helix.", 1_770_000_000_000);
    consolidator(
        home.path(),
        &json!({"changes": [{"action": "resolve", "ids": [vim, helix], "reason": "editor changed"}]}),
    );

    let out = run(home.path(), &["memory", "consolidate"]);
    let diff = ok(&out);
    assert!(diff.contains("-Uses vim."), "{diff}");
    assert!(!diff.contains("-Switched to Helix."), "{diff}");
    assert!(stderr(&out).contains("--apply"), "{}", stderr(&out));
    assert_eq!(contents(home.path()), ["Uses vim.", "Switched to Helix."]);

    ok(&run(home.path(), &["memory", "consolidate", "--apply"]));
    assert_eq!(contents(home.path()), ["Switched to Helix."]);
    let log = memory_json(home.path(), &["log", "--origin", "consolidation"]);
    assert_eq!(log.as_array().unwrap().len(), 1, "{log}");
    assert_eq!(log[0]["operation"], "remove");
    assert_eq!(log[0]["before"], "Uses vim.");
}

#[test]
fn the_idle_pass_stages_its_proposal_for_approval() {
    let home = mock_home();
    let a = add(home.path(), "Prefers dark mode.", 1_740_000_000_000);
    let b = add(home.path(), "Likes dark themes.", 1_750_000_000_000);
    consolidator(
        home.path(),
        &json!({"changes": [{"action": "merge", "ids": [a, b], "content": "Prefers dark themes."}]}),
    );
    append_config(
        home.path(),
        "\n[memory.consolidation]\nenabled = true\ninterval_secs = 60\n",
    );
    // The last pass was long ago.
    db(home.path())
        .execute(
            "INSERT INTO store_meta (key, value) VALUES ('pref.memory.consolidation.last_run', '1000')",
            [],
        )
        .unwrap();
    ok(&run(home.path(), &["cron", "tick"]));
    assert_eq!(
        contents(home.path()),
        ["Prefers dark mode.", "Likes dark themes."]
    );
    let pending = memory_json(home.path(), &["pending"]);
    assert_eq!(pending.as_array().unwrap().len(), 1, "{pending}");
    assert_eq!(pending[0]["origin"], "consolidation");
    let id = pending[0]["id"].as_str().unwrap();
    ok(&run(home.path(), &["memory", "approve", id]));
    assert_eq!(contents(home.path()), ["Prefers dark themes."]);
    // The interval has not passed again: the next tick asks nothing.
    ok(&run(home.path(), &["cron", "tick"]));
    assert!(
        memory_json(home.path(), &["pending"])
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn an_expired_entry_is_listed_as_dormant_and_wakes_on_request() {
    let home = mock_home();
    let id = add(
        home.path(),
        "Conference in Berlin this week.",
        1_770_000_000_000,
    );
    db(home.path())
        .execute(
            "UPDATE memory_entries SET expires_at = 1000 WHERE id = ?1",
            [&id],
        )
        .unwrap();
    assert!(
        memory_json(home.path(), &["list"])
            .as_array()
            .unwrap()
            .is_empty()
    );
    let listed = ok(&run(home.path(), &["memory", "list", "--all"]));
    assert!(listed.contains("[dormant]"), "{listed}");
    ok(&run(home.path(), &["memory", "wake", &id]));
    let listed = memory_json(home.path(), &["list"]);
    assert_eq!(listed[0]["status"], "active", "{listed}");
    let again = run(home.path(), &["memory", "wake", &id]);
    assert!(!again.status.success());
}
