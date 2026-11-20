//! `prometheus migrate rollback --run <run-id>` (spec: hermes-migration,
//! Migration rollback): a rolled-back run leaves the home as it was
//! before the migration, except items changed in Prometheus since, which
//! are kept and reported `kept_modified`.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Output, Stdio};

use common::hermes::{self, Hermes, Session};
use rusqlite::Connection;
use serde_json::Value;

fn migrate(home: &Path, hermes: &Path) -> Value {
    json(
        &common::command(home)
            .args(["--json", "migrate", "from-hermes", "--yes", "--hermes-home"])
            .arg(hermes)
            .stdin(Stdio::null())
            .output()
            .unwrap(),
    )
}

fn rollback(home: &Path, run: &str) -> Output {
    common::command(home)
        .args(["--json", "migrate", "rollback", "--yes", "--run", run])
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn json(out: &Output) -> Value {
    assert!(
        out.status.success(),
        "exit {:?}\nstdout {}\nstderr {}",
        out.status.code(),
        common::stdout(out),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(common::stdout(out).trim()).unwrap()
}

fn store(home: &Path) -> Connection {
    Connection::open(home.join("profiles/default/state.db")).unwrap()
}

fn sessions(home: &Path) -> Vec<String> {
    let db = store(home);
    let mut stmt = db.prepare("SELECT id FROM sessions ORDER BY id").unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// The memory entries (none before the store exists).
fn memories(home: &Path) -> Vec<String> {
    let db = store(home);
    let Ok(mut stmt) = db.prepare("SELECT content FROM memory_entries ORDER BY content") else {
        return Vec::new();
    };
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// A Hermes home with config, a secret, memory, identity, a skill and
/// `n` sessions of two messages.
fn hermes_home(dir: &Path, n: usize) -> Hermes {
    let h = Hermes::new(&dir.join("hermes"));
    h.write(
        "default",
        "config.yaml",
        "_config_version: 52\nmax_turns: 60\nmodel:\n  default: claude-sonnet-4\n  provider: anthropic\n",
    );
    h.write("default", ".env", "ANTHROPIC_API_KEY=sk-ant-test-1\n");
    h.write(
        "default",
        "memories/MEMORY.md",
        "uses tabs\n§\nprefers rust\n",
    );
    h.write("default", "SOUL.md", "You are calm.\n");
    h.write(
        "default",
        "skills/notes/SKILL.md",
        "---\nname: notes\ndescription: Take notes.\n---\nWrite notes down.\n",
    );
    let db = h.db("default", 32);
    db.execute_batch("BEGIN").unwrap();
    for i in 0..n {
        let id = format!("s{i:05}");
        #[allow(clippy::cast_precision_loss)]
        let at = 1_772_360_000.0 + i as f64 * 10.0;
        hermes::add_session(
            &db,
            &Session {
                id: &id,
                started_at: at,
                title: Some(&id),
                ..Session::default()
            },
        );
        hermes::add_message(&db, &id, "user", &format!("hello from {id}"), at + 1.0);
        hermes::add_message(&db, &id, "assistant", "hi", at + 2.0);
    }
    db.execute_batch("COMMIT").unwrap();
    h
}

/// Every file of the profile with its content, except the database
/// (whose pages change on any write), logs, caches, the store's
/// `profile_id` and the automatic `config.toml` backups every config
/// write keeps.
fn files(profile: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let skipped = ["state.db", "logs", "cache", "profile_id", "backups"];
            if skipped.iter().any(|s| rel.starts_with(s)) {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(profile, profile, &mut out);
    out
}

/// Scenario: Rollback keeps a continued session.
#[test]
fn rollback_removes_the_imported_sessions_but_keeps_a_continued_one() {
    let tmp = common::temp_home();
    let h = hermes_home(tmp.path(), 10);
    let home = tmp.path().join("prometheus");
    let report = migrate(&home, &h.root);
    let run = report["run_id"].as_str().unwrap().to_owned();
    let imported = sessions(&home);
    assert_eq!(imported.len(), 10);
    let continued = imported[3].clone();
    let out = common::command(&home)
        .args([
            "-z",
            "and the second one?",
            "--resume",
            &continued,
            "--provider",
            "mock",
            "--model",
            "mock",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let rolled = json(&rollback(&home, &run));
    let items = rolled["rolled_back"][0]["items"].as_array().unwrap();
    let of = |kind: &str, disposition: &str| {
        items
            .iter()
            .filter(|i| i["kind"] == kind && i["disposition"] == disposition)
            .count()
    };
    assert_eq!(of("session", "removed"), 9, "{items:#?}");
    assert_eq!(of("session", "kept_modified"), 1, "{items:#?}");
    let kept = items
        .iter()
        .find(|i| i["kind"] == "session" && i["disposition"] == "kept_modified")
        .unwrap();
    assert_eq!(kept["item"], continued.as_str());
    let left = sessions(&home);
    // The continued session stays; the one-shot that continued it is the
    // same session.
    assert_eq!(left, [continued]);
}

/// Task 72.12: a rollback restores the pre-migration home, the run can
/// be imported again, and a run cannot be rolled back twice.
#[test]
fn rollback_restores_the_pre_migration_home() {
    let tmp = common::temp_home();
    let h = hermes_home(tmp.path(), 3);
    let home = tmp.path().join("prometheus");
    // The home as it was: a profile with a setting and a secret of its
    // own that the migration replaces (--overwrite).
    common::ok(&common::run(
        &home,
        &["config", "set", "agent.max_iterations", "45"],
    ));
    common::ok(&common::run_with_stdin(
        &home,
        &["secrets", "set", "ANTHROPIC_API_KEY"],
        "sk-ant-mine",
    ));
    let profile = home.join("profiles/default");
    let before_files = files(&profile);
    let before_memories = memories(&home);
    let report = json(
        &common::command(&home)
            .args([
                "--json",
                "migrate",
                "from-hermes",
                "--yes",
                "--overwrite",
                "--hermes-home",
            ])
            .arg(&h.root)
            .stdin(Stdio::null())
            .output()
            .unwrap(),
    );
    let run = report["run_id"].as_str().unwrap().to_owned();
    assert_eq!(sessions(&home).len(), 3);
    assert_ne!(files(&profile), before_files, "the migration wrote files");

    let rolled = json(&rollback(&home, &run));
    let items = rolled["rolled_back"][0]["items"].as_array().unwrap();
    assert!(
        !items.iter().any(|i| i["disposition"] == "kept_modified"),
        "{items:#?}"
    );
    assert!(sessions(&home).is_empty());
    assert_eq!(memories(&home), before_memories);
    let after = files(&profile);
    // Only the rollback's own record remains besides what was there.
    let mut extra: Vec<&String> = after
        .keys()
        .filter(|k| !before_files.contains_key(*k))
        .collect();
    extra.retain(|k| !k.starts_with("migrations/hermes-"));
    assert!(extra.is_empty(), "left behind: {extra:?}");
    for (path, bytes) in &before_files {
        assert_eq!(
            after.get(path),
            Some(bytes),
            "{path} differs after the rollback"
        );
    }
    // `secrets.toml` (the file backend) compared equal above: the
    // replaced key holds `sk-ant-mine` again, and no backup is left.
    let names = common::ok(&common::run(&home, &["secrets", "list"]));
    assert!(!names.contains("PMT_MIGRATE_UNDO"), "{names}");

    // The source imports again, and the rolled-back run is gone.
    let again = migrate(&home, &h.root);
    assert_eq!(sessions(&home).len(), 3, "{again:#}");
    let out = rollback(&home, &run);
    assert_eq!(out.status.code(), Some(66), "{}", common::stdout(&out));
}

/// Only the latest run of a source can be rolled back.
#[test]
fn an_earlier_run_waits_for_the_later_one() {
    let tmp = common::temp_home();
    let h = hermes_home(tmp.path(), 1);
    let home = tmp.path().join("prometheus");
    let first = migrate(&home, &h.root)["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let second = migrate(&home, &h.root)["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = rollback(&home, &first);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains(&format!("run {second} migrated the same source")),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    json(&rollback(&home, &second));
    json(&rollback(&home, &first));
    assert!(sessions(&home).is_empty());
}

/// Messages a re-run appended to a session an earlier run imported are
/// taken back with that re-run.
#[test]
fn a_rolled_back_rerun_takes_back_the_messages_it_appended() {
    let tmp = common::temp_home();
    let h = hermes_home(tmp.path(), 1);
    let home = tmp.path().join("prometheus");
    migrate(&home, &h.root);
    let count = |home: &Path| -> i64 {
        store(home)
            .query_row("SELECT message_count FROM sessions", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(count(&home), 2);
    let db = h.db("default", 32);
    hermes::add_message(&db, "s00000", "user", "one more thing", 1_772_360_100.0);
    hermes::add_message(&db, "s00000", "assistant", "sure", 1_772_360_101.0);
    drop(db);
    let rerun = migrate(&home, &h.root)["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(count(&home), 4);
    let rolled = json(&rollback(&home, &rerun));
    let items = rolled["rolled_back"][0]["items"].as_array().unwrap();
    assert!(
        items
            .iter()
            .any(|i| i["kind"] == "appended" && i["disposition"] == "restored"),
        "{items:#?}"
    );
    assert_eq!(count(&home), 2);
    let texts: Vec<String> = {
        let db = store(&home);
        let mut stmt = db
            .prepare("SELECT content FROM messages ORDER BY seq")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(texts, ["hello from s00000", "hi"]);
    // The next run appends them again.
    migrate(&home, &h.root);
    assert_eq!(count(&home), 4);
}
