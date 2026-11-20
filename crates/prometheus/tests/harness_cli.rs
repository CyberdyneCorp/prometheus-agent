//! `prometheus harness` and the harness of sessions on the mock provider
//! (spec: harness-evolution, Opt-in per profile; Harness tree and on-disk
//! layout; Harness validation; Harness loaded once per session; Harness
//! history and diff; Rollback and manual edits).

mod common;

use std::path::{Path, PathBuf};

use common::{mock_home, newest_session, ok, run, stderr, temp_home};
use serde_json::Value;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/harness")
        .join(name)
}

fn profile(home: &Path) -> PathBuf {
    home.join("profiles/default")
}

fn json(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend(args);
    serde_json::from_str(&ok(&run(home, &all))).unwrap()
}

/// A tree with `system` as its `SYSTEM.md`, written under `home`.
fn tree(home: &Path, name: &str, system: &str) -> PathBuf {
    let dir = home.join("trees").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SYSTEM.md"), system).unwrap();
    dir
}

fn active(home: &Path) -> String {
    std::fs::read_to_string(profile(home).join("harness/ACTIVE"))
        .unwrap()
        .trim()
        .to_owned()
}

/// Scenario: Fresh profile.
#[test]
fn a_fresh_profile_activates_the_builtin_version_with_empty_always_on_files() {
    let home = temp_home();
    ok(&run(home.path(), &["profile", "create", "work"]));
    let harness = home.path().join("profiles/work/harness");
    let id = std::fs::read_to_string(harness.join("ACTIVE")).unwrap();
    let id = id.trim();
    assert!(id.starts_with("h1-") && id.len() == 19, "{id}");
    let version: toml::Table = toml::from_str(
        &std::fs::read_to_string(harness.join(format!("versions/{id}/version.toml"))).unwrap(),
    )
    .unwrap();
    assert_eq!(version["source"].as_str(), Some("builtin"));
    for file in [
        "SYSTEM.md",
        "GUARDRAILS.md",
        "MEMORY_POLICY.md",
        "TOOL_GUIDANCE.md",
    ] {
        let path = harness.join(format!("versions/{id}/tree/{file}"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "", "{file}");
    }
}

/// Scenario: Harness inspection without evolution.
#[test]
fn the_log_works_on_a_profile_without_evolution() {
    let home = temp_home();
    let out = ok(&run(home.path(), &["harness", "log"]));
    assert!(out.starts_with("* h1-"), "{out}");
    assert!(out.contains("builtin"), "{out}");
}

/// `prometheus harness validate` on the fixtures.
#[test]
fn validate_accepts_the_valid_fixture_and_lists_every_error_of_the_others() {
    let home = temp_home();
    let valid = ok(&run(
        home.path(),
        &["harness", "validate", fixture("valid").to_str().unwrap()],
    ));
    assert!(valid.starts_with("valid: 7 files"), "{valid}");
    assert!(
        valid.contains("1 skills, 1 agents, 2 scripts, 1 hooks"),
        "{valid}"
    );
    assert!(valid.contains("trigger line of skill 'recon'"), "{valid}");

    // Scenario: Missing referenced script.
    let out = run(
        home.path(),
        &[
            "harness",
            "validate",
            fixture("missing-script").to_str().unwrap(),
        ],
    );
    assert_eq!(out.status.code(), Some(65), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("scripts/clean.py"),
        "{}",
        stderr(&out)
    );

    let out = run(
        home.path(),
        &["harness", "validate", fixture("broken").to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(65));
    let errors = stderr(&out);
    for expected in [
        "off-surface paths: memories/MEMORY.md",
        "SYSTEM.md contains template syntax",
        "scripts/fix.py: line 1",
        "unknown key 'network' in hooks.toml",
        "unknown hook event 'on_magic'",
    ] {
        assert!(errors.contains(expected), "{expected}: {errors}");
    }
    // Nothing was activated by validating.
    assert!(active(home.path()).starts_with("h1-"));
}

/// Scenario: Rollback creates a new version.
#[test]
fn a_rollback_writes_a_new_version_with_the_old_tree() {
    let home = temp_home();
    for n in 2..=6 {
        let dir = tree(home.path(), &format!("t{n}"), &format!("Version {n}."));
        ok(&run(
            home.path(),
            &["harness", "apply", dir.to_str().unwrap()],
        ));
    }
    assert!(active(home.path()).starts_with("h6-"));
    let out = ok(&run(home.path(), &["harness", "rollback", "h4"]));
    assert!(out.contains("takes effect from the next session"), "{out}");
    let log = json(home.path(), &["harness", "log"]);
    let rows = log.as_array().unwrap();
    assert_eq!(rows.len(), 7);
    let newest = &rows[0];
    assert_eq!(newest["active"], true);
    assert_eq!(newest["version"]["source"], "rollback");
    let h4 = rows
        .iter()
        .find(|r| r["version"]["id"].as_str().unwrap().starts_with("h4-"))
        .unwrap();
    assert!(newest["version"]["id"].as_str().unwrap().starts_with("h7-"));
    assert_eq!(
        newest["version"]["fingerprint"],
        h4["version"]["fingerprint"]
    );
    for n in ["h5-", "h6-"] {
        assert!(
            rows.iter()
                .any(|r| r["version"]["id"].as_str().unwrap().starts_with(n))
        );
    }
    let activated =
        std::fs::read_to_string(profile(home.path()).join("harness/activations.jsonl")).unwrap();
    let last: Value = serde_json::from_str(activated.lines().last().unwrap()).unwrap();
    assert_eq!(last["type"], "harness.activated");
    assert_eq!(last["payload"]["source"], "rollback");
}

/// Scenario: Diff summary (through the CLI).
#[test]
fn diff_prints_the_unified_diff_and_the_summary() {
    let home = temp_home();
    let dir = tree(home.path(), "a", "");
    ok(&run(
        home.path(),
        &["harness", "apply", dir.to_str().unwrap()],
    ));
    std::fs::create_dir_all(dir.join("skills/recon")).unwrap();
    std::fs::write(
        dir.join("skills/recon/SKILL.md"),
        "---\nname: recon\ndescription: reconciling ledgers\n---\n",
    )
    .unwrap();
    std::fs::write(dir.join("SYSTEM.md"), "Use the recon skill.").unwrap();
    ok(&run(
        home.path(),
        &["harness", "apply", dir.to_str().unwrap()],
    ));
    let out = ok(&run(home.path(), &["harness", "diff", "h2", "h3"]));
    assert!(out.contains("+++ b/h3-"), "{out}");
    assert!(
        out.ends_with(
            "added: [skills/recon/SKILL.md], removed: [], modified: [SYSTEM.md], always_on_char_delta: 20\n"
        ),
        "{out}"
    );
    let shown = ok(&run(home.path(), &["harness", "show", "h3", "SYSTEM.md"]));
    assert_eq!(shown, "Use the recon skill.");
}

#[test]
fn an_invalid_apply_writes_no_version() {
    let home = temp_home();
    let out = run(
        home.path(),
        &["harness", "apply", fixture("broken").to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(65));
    assert_eq!(
        json(home.path(), &["harness", "log"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

/// The stored system prompt of `session`, its hash and recorded harness.
fn stored(home: &Path, session: &str) -> (String, String, String) {
    let db = rusqlite::Connection::open(profile(home).join("state.db")).unwrap();
    let (tiers, sha): (String, String) = db
        .query_row(
            "SELECT tiers, sha256 FROM session_prompts WHERE session_id = ?1",
            [session],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let version: String = db
        .query_row(
            "SELECT harness_version FROM sessions WHERE id = ?1",
            [session],
            |row| row.get(0),
        )
        .unwrap();
    (tiers, sha, version)
}

/// Scenario: Adoption mid-session (activated by a manual edit, the same
/// activation path an adoption takes).
#[test]
fn an_open_session_keeps_its_harness_until_an_explicit_now() {
    let home = mock_home();
    let first = tree(home.path(), "first", "Answer in French.");
    ok(&run(
        home.path(),
        &["harness", "apply", first.to_str().unwrap()],
    ));
    let h2 = active(home.path());
    ok(&run(home.path(), &["-z", "hello"]));
    let open = newest_session(home.path());
    let (tiers, sha, version) = stored(home.path(), &open);
    assert_eq!(version, h2);
    assert!(tiers.contains("Answer in French."), "{tiers}");

    let second = tree(home.path(), "second", "Answer in German.");
    ok(&run(
        home.path(),
        &["harness", "apply", second.to_str().unwrap()],
    ));
    let h3 = active(home.path());
    ok(&run(home.path(), &["chat", "-r", &open, "-z", "again"]));
    let (_, again, version) = stored(home.path(), &open);
    assert_eq!(again, sha, "byte-identical system prompt");
    assert_eq!(version, h2);

    ok(&run(home.path(), &["-z", "new session"]));
    let fresh = newest_session(home.path());
    let (tiers, _, version) = stored(home.path(), &fresh);
    assert_eq!(version, h3);
    assert!(tiers.contains("Answer in German."), "{tiers}");
}

#[test]
fn a_harness_subagent_and_skill_are_registered_for_the_session() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["harness", "apply", fixture("valid").to_str().unwrap()],
    ));
    ok(&run(home.path(), &["-z", "hello"]));
    let id = newest_session(home.path());
    let (tiers, _, _) = stored(home.path(), &id);
    // The skills index lists the harness skill; the layer carries c1-c2.
    assert!(tiers.contains("recon"), "{tiers}");
    assert!(tiers.contains("You keep ledgers balanced."), "{tiers}");
    assert!(tiers.contains("load the `recon` skill"), "{tiers}");
}

/// An explicit `--now` reaches a session that is open in another process
/// at its next turn, and logs the cache break.
#[cfg(unix)]
#[test]
fn a_now_activation_rebuilds_an_open_session_at_its_next_turn() {
    use common::pty::{exit, prompt, spawn};
    use expectrl::Expect as _;

    let home = mock_home();
    let first = tree(home.path(), "first", "Answer in French.");
    ok(&run(
        home.path(),
        &["harness", "apply", first.to_str().unwrap()],
    ));
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("hello").unwrap();
    p.expect("mock: hello").unwrap();
    prompt(&mut p);
    let open = newest_session(home.path());
    let (_, sha, _) = stored(home.path(), &open);

    let later = tree(home.path(), "later", "Answer in Latin.");
    let out = ok(&run(
        home.path(),
        &["harness", "apply", later.to_str().unwrap()],
    ));
    assert!(out.contains("takes effect from the next session"), "{out}");
    p.send_line("unchanged").unwrap();
    p.expect("mock: unchanged").unwrap();
    prompt(&mut p);
    assert_eq!(stored(home.path(), &open).1, sha, "no --now: same prompt");

    let now = tree(home.path(), "now", "Answer in Greek.");
    let out = ok(&run(
        home.path(),
        &["harness", "apply", now.to_str().unwrap(), "--now"],
    ));
    assert!(
        out.contains("1 open session(s) rebuild their prompt"),
        "{out}"
    );
    p.send_line("rebuilt").unwrap();
    p.expect("mock: rebuilt").unwrap();
    prompt(&mut p);
    exit(p);
    let (tiers, rebuilt, version) = stored(home.path(), &open);
    assert_ne!(rebuilt, sha);
    assert_eq!(version, active(home.path()));
    assert!(tiers.contains("Answer in Greek."), "{tiers}");
    let logs: String = common::files_under(home.path())
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "log" || e == "jsonl"))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect();
    assert!(
        logs.contains("prompt cache invalidated by harness change"),
        "no WARN record"
    );
}
