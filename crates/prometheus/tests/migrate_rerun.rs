//! `prometheus migrate from-hermes` run again: the ledger makes re-runs
//! idempotent and incremental, an interrupted run resumes, and the exit
//! codes (spec: hermes-migration, Idempotent incremental re-run;
//! Per-component atomicity and resumability; Migration exit codes and
//! confirmation).

mod common;

use std::path::Path;
use std::process::{Output, Stdio};

use common::hermes::{self, Hermes, Session};
use rusqlite::Connection;
use serde_json::Value;

fn migrate(home: &Path, hermes: &Path, extra: &[&str]) -> Output {
    common::command(home)
        .args(["--json", "migrate", "from-hermes", "--yes", "--hermes-home"])
        .arg(hermes)
        .args(extra)
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

fn entries(report: &Value) -> &Vec<Value> {
    report["profiles"][0]["entries"].as_array().unwrap()
}

fn entry<'a>(report: &'a Value, item: &str) -> &'a Value {
    entries(report)
        .iter()
        .find(|e| e["item"] == item)
        .unwrap_or_else(|| panic!("no entry {item}: {:#?}", entries(report)))
}

fn store(home: &Path) -> Connection {
    Connection::open(home.join("profiles/default/state.db")).unwrap()
}

fn session_count(home: &Path) -> i64 {
    store(home)
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap()
}

/// The Prometheus id and message count of the session imported from
/// Hermes session `legacy`.
fn imported(home: &Path, legacy: &str) -> (String, i64) {
    store(home)
        .query_row(
            "SELECT s.id, s.message_count FROM sessions s JOIN session_legacy_ids l \
             ON l.session_id = s.id WHERE l.legacy_id = ?1",
            [legacy],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

fn session(db: &Connection, id: &str, at: f64) {
    hermes::add_session(
        db,
        &Session {
            id,
            started_at: at,
            title: Some(id),
            ..Session::default()
        },
    );
    hermes::add_message(db, id, "user", &format!("hello from {id}"), at + 1.0);
    hermes::add_message(db, id, "assistant", "hi", at + 2.0);
}

/// A Hermes home with config, a secret, memory, a skill and `n` sessions.
fn home_with(dir: &Path, n: usize) -> Hermes {
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
        #[allow(clippy::cast_precision_loss)]
        session(&db, &format!("s{i:05}"), 1_772_360_000.0 + i as f64 * 10.0);
    }
    db.execute_batch("COMMIT").unwrap();
    h
}

#[test]
fn a_second_run_imports_nothing_new() {
    let tmp = common::temp_home();
    let h = home_with(tmp.path(), 2);
    let home = tmp.path().join("prometheus");
    json(&migrate(&home, &h.root, &[]));
    let memories = |home: &Path| -> i64 {
        store(home)
            .query_row("SELECT COUNT(*) FROM memory_entries", [], |r| r.get(0))
            .unwrap()
    };
    let before = (session_count(&home), memories(&home));
    let report = json(&migrate(&home, &h.root, &[]));
    for e in entries(&report) {
        assert!(
            !matches!(
                e["disposition"].as_str(),
                Some("mapped" | "transformed" | "secret" | "conflict" | "failed")
            ),
            "a re-run imported or flagged {e:#}"
        );
    }
    assert_eq!(
        entry(&report, "max_turns")["reason"],
        "unchanged since the last migration"
    );
    assert_eq!((session_count(&home), memories(&home)), before);
    let ledger =
        std::fs::read_to_string(home.join("profiles/default/migrations/hermes-ledger.toml"))
            .unwrap();
    assert!(ledger.contains("config:agent.max_iterations"), "{ledger}");
    assert!(
        !ledger.contains("sk-ant-test-1"),
        "the ledger holds a secret value"
    );
}

#[test]
fn a_rerun_imports_only_the_new_sessions() {
    let tmp = common::temp_home();
    let h = home_with(tmp.path(), 2);
    let home = tmp.path().join("prometheus");
    json(&migrate(&home, &h.root, &[]));
    assert_eq!(session_count(&home), 2);
    let db = h.db("default", 32);
    for (i, id) in ["n1", "n2", "n3"].iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        session(&db, id, 1_772_400_000.0 + i as f64 * 10.0);
    }
    drop(db);
    let report = json(&migrate(&home, &h.root, &["--only", "sessions"]));
    let new: Vec<&Value> = entries(&report)
        .iter()
        .filter(|e| e["disposition"] == "mapped")
        .collect();
    assert_eq!(new.len(), 3, "{:#?}", entries(&report));
    assert_eq!(entry(&report, "s00000")["disposition"], "skipped");
    assert_eq!(session_count(&home), 5);
}

#[test]
fn a_user_edit_is_kept_and_reported_as_a_conflict() {
    let tmp = common::temp_home();
    let h = home_with(tmp.path(), 0);
    let home = tmp.path().join("prometheus");
    json(&migrate(&home, &h.root, &["--only", "config"]));
    common::run(&home, &["config", "set", "agent.max_iterations", "80"]);
    h.write(
        "default",
        "config.yaml",
        "_config_version: 52\nmax_turns: 90\nmodel:\n  default: claude-opus-4\n  provider: anthropic\n",
    );
    let report = json(&migrate(&home, &h.root, &["--only", "config"]));
    let config = std::fs::read_to_string(home.join("profiles/default/config.toml")).unwrap();
    assert!(config.contains("max_iterations = 80"), "{config}");
    assert_eq!(entry(&report, "max_turns")["disposition"], "conflict");
    // A value the user left alone follows Hermes.
    assert!(config.contains("claude-opus-4"), "{config}");
    // --overwrite takes Hermes' value.
    json(&migrate(
        &home,
        &h.root,
        &["--only", "config", "--overwrite"],
    ));
    let config = std::fs::read_to_string(home.join("profiles/default/config.toml")).unwrap();
    assert!(config.contains("max_iterations = 90"), "{config}");
}

#[test]
fn a_grown_hermes_session_is_appended_unless_continued_here() {
    let tmp = common::temp_home();
    let h = home_with(tmp.path(), 2);
    let home = tmp.path().join("prometheus");
    json(&migrate(&home, &h.root, &["--only", "sessions"]));
    let (id, count) = imported(&home, "s00000");
    assert_eq!(count, 2);
    let db = h.db("default", 32);
    for (i, text) in ["one", "two", "three", "four"].iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        hermes::add_message(&db, "s00000", "user", text, 1_772_370_000.0 + i as f64);
    }
    // s00001 was continued in Prometheus: its new Hermes message stays out.
    hermes::add_message(&db, "s00001", "user", "later", 1_772_370_100.0);
    drop(db);
    let (continued, _) = imported(&home, "s00001");
    store(&home)
        .execute(
            "UPDATE sessions SET message_count = message_count + 1 WHERE id = ?1",
            [&continued],
        )
        .unwrap();
    let report = json(&migrate(&home, &h.root, &["--only", "sessions"]));
    assert_eq!(imported(&home, "s00000"), (id, 6));
    assert_eq!(entry(&report, "s00000")["disposition"], "transformed");
    assert_eq!(entry(&report, "s00001")["disposition"], "conflict");
    assert_eq!(session_count(&home), 2);
}

#[cfg(unix)]
#[test]
fn an_interrupted_run_resumes_from_the_last_committed_batch() {
    use std::io::{BufRead, BufReader};

    let tmp = common::temp_home();
    let total = 3000;
    let h = home_with(tmp.path(), total);
    let home = tmp.path().join("prometheus");
    let mut child = common::command(&home)
        .args(["--json", "migrate", "from-hermes", "--yes", "--hermes-home"])
        .arg(&h.root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = BufReader::new(child.stderr.take().unwrap());
    let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap());
    // Read stderr to the end (a closed pipe would fail the child's
    // writes); Ctrl+C right after the first committed batch.
    let reader = std::thread::spawn(move || {
        let mut sent = false;
        for line in stderr.lines() {
            if !sent && line.unwrap().starts_with("sessions: 500/") {
                nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT).unwrap();
                sent = true;
            }
        }
        sent
    });
    let out = child.wait_with_output().unwrap();
    assert!(reader.join().unwrap(), "no batch was committed");
    assert_eq!(
        out.status.code(),
        Some(130),
        "{}\n{}",
        common::stdout(&out),
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_str(common::stdout(&out).trim()).unwrap();
    assert_eq!(report["interrupted"], true);
    let committed = session_count(&home);
    assert!(
        committed > 0 && committed < 3000 && committed % 500 == 0,
        "{committed} sessions: not a whole number of batches"
    );
    let status = |report: &Value, c: &str| {
        report["profiles"][0]["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["component"] == c)
            .map(|x| x["status"].clone())
    };
    assert_eq!(status(&report, "config"), Some(Value::from("committed")));
    assert_eq!(
        status(&report, "sessions"),
        Some(Value::from("interrupted"))
    );
    assert_eq!(status(&report, "cron"), Some(Value::from("interrupted")));

    let report = json(&migrate(&home, &h.root, &[]));
    for item in ["max_turns", ".env:ANTHROPIC_API_KEY", "memories/MEMORY.md"] {
        assert_eq!(
            entry(&report, item)["reason"],
            "unchanged since the last migration",
            "{item} was imported again"
        );
    }
    let mapped = entries(&report)
        .iter()
        .filter(|e| e["component"] == "sessions" && e["disposition"] == "mapped")
        .count();
    assert_eq!(i64::try_from(mapped).unwrap(), 3000 - committed);
    assert_eq!(session_count(&home), 3000);
}

#[cfg(unix)]
#[test]
fn an_unwritable_destination_exits_73() {
    use std::os::unix::fs::PermissionsExt;

    if nix::unistd::geteuid().is_root() {
        return; // root writes anywhere
    }
    let tmp = common::temp_home();
    let h = home_with(tmp.path(), 0);
    let home = tmp.path().join("prometheus");
    std::fs::create_dir_all(common::user_home(&home)).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o500)).unwrap();
    let out = migrate(&home, &h.root, &[]);
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(out.status.code(), Some(73), "{}", common::stdout(&out));
    let report: Value = serde_json::from_str(common::stdout(&out).trim()).unwrap();
    assert_eq!(report["profiles"][0]["code"], "migrate.destination");
}

#[cfg(feature = "gateway")]
#[test]
fn a_sender_revoked_in_prometheus_stays_revoked_after_a_rerun() {
    let tmp = common::temp_home();
    let h = home_with(tmp.path(), 0);
    h.write("default", ".env", "TELEGRAM_BOT_TOKEN=123:abc\n");
    h.write(
        "default",
        "platforms/pairing/telegram-approved.json",
        r#"{"424242": {"user_name": "ada", "approved_at": 1772360100.0}}"#,
    );
    let home = tmp.path().join("prometheus");
    let first = json(&migrate(&home, &h.root, &["--only", "secrets,gateway"]));
    assert_eq!(
        entry(&first, "pairing:telegram:424242")["disposition"],
        "mapped"
    );
    let revoke = common::run(&home, &["pairing", "revoke", "telegram", "424242"]);
    assert!(revoke.status.success(), "{}", common::stdout(&revoke));

    let again = json(&migrate(&home, &h.root, &["--only", "secrets,gateway"]));
    let pairing = entry(&again, "pairing:telegram:424242");
    assert_eq!(pairing["disposition"], "skipped", "{pairing:#}");
    assert_eq!(pairing["reason"], "unchanged since the last migration");
    let approved: i64 = store(&home)
        .query_row(
            "SELECT COUNT(*) FROM gateway_pairing WHERE sender_id = '424242'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(approved, 0, "a re-run approved a revoked sender again");
}

#[cfg(unix)]
#[test]
fn a_component_failing_midway_leaves_none_of_its_output() {
    use std::os::unix::fs::PermissionsExt;

    if nix::unistd::geteuid().is_root() {
        return; // root writes anywhere
    }
    let tmp = common::temp_home();
    let h = home_with(tmp.path(), 0);
    h.write(
        "default",
        "skills/zeta/late/SKILL.md",
        "---\nname: late\ndescription: Copied last.\n---\nDo it late.\n",
    );
    let home = tmp.path().join("prometheus");
    json(&migrate(&home, &h.root, &["--only", "config"]));
    // The second skill cannot be written: the first one is already
    // copied when the component fails.
    let skills = home.join("profiles/default/skills");
    let blocked = skills.join("zeta");
    std::fs::create_dir_all(&blocked).unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o500)).unwrap();
    let out = migrate(&home, &h.root, &[]);
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", common::stdout(&out));
    let report: Value = serde_json::from_str(common::stdout(&out).trim()).unwrap();
    let component = |name: &str| {
        report["profiles"][0]["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["component"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(component("skills")["status"], "failed", "{report:#}");
    assert_eq!(component("memory")["status"], "committed");
    assert_eq!(entry(&report, "skills/notes")["disposition"], "failed");
    assert!(
        !skills.join("notes").exists(),
        "a failed component left a skill"
    );
    let ledger =
        std::fs::read_to_string(home.join("profiles/default/migrations/hermes-ledger.toml"))
            .unwrap();
    assert!(!ledger.contains("skill:"), "{ledger}");

    let again = json(&migrate(&home, &h.root, &[]));
    assert_eq!(entry(&again, "skills/notes")["disposition"], "mapped");
    assert!(skills.join("notes/SKILL.md").is_file());
    assert!(skills.join("zeta/late/SKILL.md").is_file());
}

#[test]
fn a_session_deleted_in_prometheus_is_not_imported_again() {
    let tmp = common::temp_home();
    let h = home_with(tmp.path(), 2);
    let home = tmp.path().join("prometheus");
    json(&migrate(&home, &h.root, &["--only", "sessions"]));
    let (id, _) = imported(&home, "s00000");
    let deleted = common::run(&home, &["sessions", "delete", &id, "--yes"]);
    assert!(deleted.status.success(), "{}", common::stdout(&deleted));

    let again = json(&migrate(&home, &h.root, &["--only", "sessions"]));
    let entry = entry(&again, "s00000");
    assert_eq!(entry["disposition"], "skipped", "{entry:#}");
    assert_eq!(session_count(&home), 1);
}
