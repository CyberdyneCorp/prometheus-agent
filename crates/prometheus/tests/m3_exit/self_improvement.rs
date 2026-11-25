//! Task 81.2: the offline self-improvement demo adopts a harness proposal
//! and writes an experience entry, a later session of the domain receives
//! the lesson, and the answer-free tripwire keeps grader material out
//! (spec: replay-and-evaluation, Offline self-improvement demo;
//! harness-evolution, Proposals and adoption policy, Harness versions
//! apply from the next session, Leak globs and verdict tripwire;
//! experience-library, Entries are written on adoption only, Lessons
//! offered to later sessions, Answer-free and secret-free entries, Export
//! and import between profiles).

use std::path::Path;

use serde_json::Value;

use crate::common::{self, files_under, ok, run, stderr, stdout, temp_home};

/// The demo's expected deliverables: grader material, never part of a
/// harness or a lesson.
const ANSWERS: [&str; 2] = ["85.15", "20.25"];

fn json(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    serde_json::from_str(&ok(&run(home, &all))).unwrap()
}

fn set(home: &Path, profile: &str, key: &str, value: &str) {
    ok(&run(home, &["-p", profile, "config", "set", key, value]));
}

/// The adopted proposal (the demo's policy adopts one per domain; the
/// other task's selection kept the base, `no-change`).
fn adopted(home: &Path) -> Value {
    let proposals = json(home, &["evolve", "proposals"]);
    let adopted: Vec<&Value> = proposals
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["status"] == "adopted")
        .collect();
    assert_eq!(adopted.len(), 1, "{proposals}");
    adopted[0].clone()
}

/// Every file under `dir` holding none of `needles`.
fn assert_free_of(dir: &Path, needles: &[&str]) {
    for file in files_under(dir) {
        let text = String::from_utf8_lossy(&std::fs::read(&file).unwrap()).into_owned();
        for needle in needles {
            assert!(
                !text.contains(needle),
                "{} holds {needle:?}",
                file.display()
            );
        }
    }
}

fn session_row(home: &Path, id: &str, column: &str) -> String {
    let db = rusqlite::Connection::open_with_flags(
        home.join("profiles/default/state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let sql = if column == "tiers" {
        "SELECT tiers FROM session_prompts WHERE session_id = ?1"
    } else {
        "SELECT harness_version FROM sessions WHERE id = ?1"
    };
    db.query_row(sql, [id], |row| row.get(0)).unwrap()
}

#[test]
fn the_demo_adopts_a_lesson_that_a_later_session_receives_answer_free() {
    let root = temp_home();
    let out = root.path().join("demo");
    let demo = common::command(root.path())
        .args(["demo", "--evolve", "--output", out.to_str().unwrap()])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let text = stdout(&demo);
    assert_eq!(demo.status.code(), Some(0), "{text}{}", stderr(&demo));
    assert!(text.contains("demo: 2 tasks completed; 0 failed"), "{text}");

    // The demo's own profile: the proposal adopted as the active harness,
    // and its lesson filed under the domain.
    let home = out.join("home");
    let home = home.as_path();
    set(home, "default", "evolve.enabled", "true");
    let proposal = adopted(home);
    let version = proposal["adopted_version"].as_str().unwrap().to_owned();
    let log = ok(&run(home, &["harness", "log"]));
    let active = log.lines().next().unwrap_or_default();
    assert!(
        active.starts_with(&format!("* {version}")) && active.contains("evolve"),
        "{log}"
    );
    let entries = json(home, &["experience", "list", "--domain", "accounting"]);
    let entries = entries.as_array().unwrap();
    assert_eq!(entries.len(), 1, "{entries:?}");
    let entry = &entries[0];
    assert_eq!(entry["proposal_id"], proposal["id"]);
    assert_eq!(entry["harness_version"], version.as_str());
    assert_eq!(entry["times_shown"], 0);
    let lesson = entry["lesson"].as_str().unwrap().to_owned();

    // Answer-free: no expected deliverable and no verdict reached the
    // harness versions or the library.
    let profile = home.join("profiles/default");
    let mut grader = ANSWERS.to_vec();
    grader.push("pmt-eval-verdict");
    assert_free_of(&profile.join("experience"), &grader);
    assert_free_of(&profile.join("harness/versions"), &grader);

    // A later session of the domain runs the adopted harness and starts
    // with the lesson.
    for (key, value) in [
        ("model.provider", "mock"),
        ("model.default", "mock"),
        ("experience.session_lessons", "true"),
    ] {
        set(home, "default", key, value);
    }
    ok(&run(
        home,
        &["-z", "total this month's ledger", "--domain", "accounting"],
    ));
    let session = common::newest_session(home);
    assert_eq!(session_row(home, &session, "harness_version"), version);
    let tiers = session_row(home, &session, "tiers");
    assert!(
        tiers.contains("## Lessons from earlier work in accounting") && tiers.contains(&lesson),
        "{tiers}"
    );
    let shown = json(home, &["experience", "list", "--domain", "accounting"]);
    assert_eq!(shown[0]["times_shown"], 1);

    // The tripwire: an entry that carries a verdict is refused whole when
    // it moves to another profile, and nothing is written there.
    let id = entry["entry_id"].as_str().unwrap();
    let notes = profile
        .join("experience/accounting/entries")
        .join(id)
        .join("EXPERIENCE.md");
    let mut text = std::fs::read_to_string(&notes).unwrap();
    text.push_str("\nThe graded run: {\"pass_rate\": 1.0}\n");
    std::fs::write(&notes, text).unwrap();
    let archive = root.path().join("leaky.tar.zst");
    ok(&run(
        home,
        &[
            "experience",
            "export",
            "accounting",
            archive.to_str().unwrap(),
        ],
    ));
    ok(&run(home, &["profile", "create", "other", "--no-alias"]));
    set(home, "other", "evolve.enabled", "true");
    let refused = run(
        home,
        &[
            "-p",
            "other",
            "experience",
            "import",
            archive.to_str().unwrap(),
        ],
    );
    assert_eq!(refused.status.code(), Some(65), "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("verdict tripwire"),
        "{}",
        stderr(&refused)
    );
    let other = json(home, &["-p", "other", "experience", "list", "--all"]);
    assert_eq!(other, Value::Array(Vec::new()), "nothing imported");
}
