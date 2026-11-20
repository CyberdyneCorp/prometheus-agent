//! The experience library end to end on the mock provider (spec:
//! experience-library, Library layout; Experience distillation; Entries
//! are written on adoption only; Domain resolution for a job; Lessons
//! offered to later sessions; Rollback demotes entries; Library CLI;
//! Export and import between profiles; Restore on resume and repair;
//! Library events).

#![cfg(feature = "evolve")]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{append_config, mock_home, newest_session, ok, run, stderr, stdout};
use serde_json::Value;

const MOCK: &str = include_str!("fixtures/evolve/mock.toml");

fn json(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend(args);
    serde_json::from_str(&ok(&run(home, &all))).unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(["-c", "commit.gpgsign=false", "-C"])
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A home with evolution on, the evolution mock script and a git
/// workspace holding `ledger.csv`.
fn setup(extra_config: &str) -> tempfile::TempDir {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "evolve.enabled", "true"],
    ));
    std::fs::write(home.path().join("profiles/default/mock.toml"), MOCK).unwrap();
    append_config(
        home.path(),
        &format!("\n[providers.mock]\nscript = \"mock.toml\"\n{extra_config}"),
    );
    let workspace = home.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("ledger.csv"), "amount\n10.00\n").unwrap();
    git(&workspace, &["init", "-q"]);
    git(&workspace, &["add", "-A"]);
    git(&workspace, &["commit", "-q", "-m", "init"]);
    home
}

/// Runs a k=1 job on a task file naming `domain` (none: classified); the
/// pending proposal's id.
fn evolve(home: &Path, statement: &str, domain: Option<&str>, flags: &[&str]) -> String {
    let task = home.join(format!("task-{}.toml", ulid::Ulid::generate()));
    let named = domain
        .map(|d| format!("domain = \"{d}\"\n"))
        .unwrap_or_default();
    std::fs::write(
        &task,
        format!("statement = \"{statement}\"\n{named}workspace = \"ws\"\n"),
    )
    .unwrap();
    let mut args = vec![
        "evolve",
        "run",
        "--task",
        task.to_str().unwrap(),
        "--k",
        "1",
    ];
    args.extend_from_slice(flags);
    let out = run(home, &args);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    let proposals = json(home, &["evolve", "proposals"]);
    proposals.as_array().unwrap().last().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn profile(home: &Path) -> PathBuf {
    home.join("profiles/default")
}

fn entries(home: &Path, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["experience", "list"];
    all.extend_from_slice(args);
    json(home, &all).as_array().unwrap().clone()
}

fn entry_dir(home: &Path, domain: &str, id: &str) -> PathBuf {
    profile(home)
        .join("experience")
        .join(domain)
        .join("entries")
        .join(id)
}

/// Scenarios: Entry created (from a mock-provider job); Library events.
#[test]
fn a_mock_job_distils_a_lesson_that_adoption_files_in_its_domain() {
    let home = setup("");
    let id = evolve(
        home.path(),
        "TASK-SUM Reconcile Q3 ledger",
        Some("Accounting"),
        &[],
    );
    let review = ok(&run(home.path(), &["evolve", "review", &id]));
    assert!(
        review.contains("lesson (accounting): state the total with two decimals")
            && review.contains("confidence: high (because every rollout agrees)"),
        "{review}"
    );
    assert!(
        entries(home.path(), &[]).is_empty(),
        "nothing before adoption"
    );
    let adopted = ok(&run(home.path(), &["evolve", "adopt", &id]));
    assert!(
        adopted.contains("experience entry ") && adopted.contains(" added to domain accounting"),
        "{adopted}"
    );
    let listed = entries(home.path(), &[]);
    assert_eq!(listed.len(), 1);
    let entry = listed[0]["entry_id"].as_str().unwrap();
    let re = regex::Regex::new(r"^\d{8}-task-sum-reconcile-q3-ledger-[0-9a-f]{6}$").unwrap();
    assert!(re.is_match(entry), "{entry}");
    let dir = entry_dir(home.path(), "accounting", entry);
    for part in ["EXPERIENCE.md", "entry.toml", "harness/GUARDRAILS.md"] {
        assert!(dir.join(part).is_file(), "{part}");
    }
    let shown = ok(&run(home.path(), &["experience", "show", entry]));
    assert!(
        shown.contains("## Transferable lesson") && shown.contains("- cue: ledger totals"),
        "{shown}"
    );
    let domains = ok(&run(home.path(), &["experience", "domains"]));
    assert_eq!(domains, "accounting  1 entry\n");
    let events =
        std::fs::read_to_string(profile(home.path()).join("experience/events.jsonl")).unwrap();
    assert!(
        events.contains("\"type\":\"experience.entry.added\""),
        "{events}"
    );
    let jobs = json(home.path(), &["evolve", "list"]);
    let job = jobs[0]["id"].as_str().unwrap();
    let log = std::fs::read_to_string(
        profile(home.path())
            .join("evolve/jobs")
            .join(job)
            .join("events.jsonl"),
    )
    .unwrap();
    assert!(
        log.contains("\"type\":\"experience.lesson.distilled\""),
        "{log}"
    );
}

/// Scenario: Rejected proposal (the task 70.2 verification).
#[test]
fn a_rejected_proposal_writes_no_entry() {
    let home = setup("");
    let id = evolve(home.path(), "TASK-SUM the ledger", Some("accounting"), &[]);
    let proposal = json(home.path(), &["evolve", "proposals"])[0].clone();
    assert!(proposal["lesson"].is_object(), "{proposal}");
    ok(&run(home.path(), &["evolve", "reject", &id]));
    assert!(entries(home.path(), &["--all"]).is_empty());
    let dir = profile(home.path()).join("experience/accounting/entries");
    assert!(!dir.exists() || std::fs::read_dir(&dir).unwrap().next().is_none());
}

/// Scenario: Classifier limited to known domains (the mock answers one).
#[test]
fn a_task_without_a_domain_is_classified_among_existing_domains() {
    let home = setup("");
    for name in ["accounting", "devops"] {
        std::fs::create_dir_all(profile(home.path()).join("experience").join(name)).unwrap();
    }
    evolve(home.path(), "TASK-SUM unfiled", None, &[]);
    let jobs = json(home.path(), &["evolve", "list"]);
    assert_eq!(jobs[0]["domain"], "accounting");
    let job = jobs[0]["id"].as_str().unwrap();
    let meta = std::fs::read_to_string(
        profile(home.path())
            .join("evolve/jobs")
            .join(job)
            .join("task/task.toml"),
    )
    .unwrap();
    assert!(meta.contains("domain_source = \"classifier\""), "{meta}");
}

/// Scenarios: Forget an entry; Rolling back an adopted harness; restore;
/// pin; an unknown entry.
#[test]
fn entries_are_pinned_forgotten_demoted_and_restored_from_the_cli() {
    let home = setup("");
    let h1 = json(home.path(), &["harness", "log"])[0]["version"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = evolve(home.path(), "TASK-SUM first", Some("accounting"), &[]);
    ok(&run(home.path(), &["evolve", "adopt", &id]));
    let entry = entries(home.path(), &[])[0]["entry_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let missing = run(home.path(), &["experience", "show", "nope"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(
        stderr(&missing).contains("no experience entry 'nope'"),
        "{}",
        stderr(&missing)
    );

    ok(&run(home.path(), &["experience", "pin", &entry]));
    assert_eq!(entries(home.path(), &[])[0]["pinned"], true);
    ok(&run(home.path(), &["experience", "unpin", &entry]));

    let rolled = ok(&run(home.path(), &["harness", "rollback", &h1]));
    assert!(
        rolled.contains(&format!("experience entry {entry} rolled back")),
        "{rolled}"
    );
    assert!(entries(home.path(), &[]).is_empty(), "hidden");
    let all = entries(home.path(), &["--all"]);
    assert_eq!(all[0]["status"], "rolled-back");
    ok(&run(home.path(), &["experience", "restore", &entry]));
    assert_eq!(entries(home.path(), &[])[0]["status"], "active");

    let versions = json(home.path(), &["harness", "log"])
        .as_array()
        .unwrap()
        .len();
    ok(&run(home.path(), &["experience", "forget", &entry]));
    assert!(!entry_dir(home.path(), "accounting", &entry).exists());
    let index =
        std::fs::read_to_string(profile(home.path()).join("experience/accounting/INDEX.md"))
            .unwrap();
    assert!(
        index.contains("_(empty — no adopted lessons in this domain yet)_"),
        "{index}"
    );
    assert_eq!(
        json(home.path(), &["harness", "log"])
            .as_array()
            .unwrap()
            .len(),
        versions
    );
}

/// Scenario: Deleted library directory (no model is reachable, so the
/// rebuild provably makes no model call).
#[test]
fn a_deleted_domain_is_rebuilt_without_a_model() {
    let home = setup("");
    let h1 = json(home.path(), &["harness", "log"])[0]["version"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    for statement in ["TASK-SUM one", "TASK-SUM two"] {
        // Each job starts from the built-in harness, so each proposal
        // changes it.
        ok(&run(home.path(), &["harness", "rollback", &h1]));
        let id = evolve(home.path(), statement, Some("accounting"), &[]);
        ok(&run(home.path(), &["evolve", "adopt", &id]));
    }
    let before: Vec<String> = entries(home.path(), &["--all"])
        .iter()
        .map(|e| e["entry_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(before.len(), 2);
    std::fs::remove_dir_all(profile(home.path()).join("experience/accounting")).unwrap();
    std::fs::remove_file(profile(home.path()).join("mock.toml")).unwrap();
    let rebuilt = ok(&run(
        home.path(),
        &["experience", "rebuild", "--domain", "accounting"],
    ));
    assert!(rebuilt.starts_with("restored 2 entries"), "{rebuilt}");
    let mut after: Vec<String> = entries(home.path(), &[])
        .iter()
        .map(|e| e["entry_id"].as_str().unwrap().to_owned())
        .collect();
    after.sort();
    let mut before = before;
    before.sort();
    assert_eq!(after, before);
}

/// Scenario: Archive with a secret; a clean archive moves to another
/// profile with new ids.
#[test]
fn archives_move_between_profiles_and_a_secret_refuses_the_whole_archive() {
    let home = setup("");
    let id = evolve(home.path(), "TASK-SUM export me", Some("accounting"), &[]);
    ok(&run(home.path(), &["evolve", "adopt", &id]));
    let entry = entries(home.path(), &[])[0]["entry_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let clean = home.path().join("clean.tar.zst");
    ok(&run(
        home.path(),
        &[
            "experience",
            "export",
            "accounting",
            clean.to_str().unwrap(),
        ],
    ));

    ok(&run(home.path(), &["profile", "create", "other"]));
    ok(&run(
        home.path(),
        &["-p", "other", "config", "set", "evolve.enabled", "true"],
    ));
    let imported = ok(&run(
        home.path(),
        &[
            "-p",
            "other",
            "experience",
            "import",
            clean.to_str().unwrap(),
            "--domain",
            "Finance Ops",
        ],
    ));
    assert!(imported.starts_with("imported 1 entry"), "{imported}");
    let other = json(home.path(), &["-p", "other", "experience", "list"]);
    assert_eq!(other[0]["domain"], "finance-ops");
    assert_ne!(other[0]["entry_id"].as_str().unwrap(), entry);
    assert!(
        other[0]["imported_from"]
            .as_str()
            .unwrap()
            .ends_with("clean.tar.zst")
    );

    let skill = entry_dir(home.path(), "accounting", &entry).join("harness/skills/recon/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(
        &skill,
        "---\nname: recon\ndescription: reconciling ledgers\n---\nUse sk-live-abcdef1234567890 for the API.\n",
    )
    .unwrap();
    let leaky = home.path().join("leaky.tar.zst");
    ok(&run(
        home.path(),
        &[
            "experience",
            "export",
            "accounting",
            leaky.to_str().unwrap(),
        ],
    ));
    let before = json(home.path(), &["-p", "other", "experience", "list", "--all"]);
    let refused = run(
        home.path(),
        &[
            "-p",
            "other",
            "experience",
            "import",
            leaky.to_str().unwrap(),
        ],
    );
    assert_eq!(refused.status.code(), Some(65), "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains(&format!("entry {entry}")),
        "{}",
        stderr(&refused)
    );
    assert_eq!(
        json(home.path(), &["-p", "other", "experience", "list", "--all"]),
        before
    );
}

/// Writes an entry of `domain` straight into the library.
fn plant(home: &Path, domain: &str, id: &str, confidence: &str, at: &str) {
    let dir = entry_dir(home, domain, id);
    std::fs::create_dir_all(dir.join("harness")).unwrap();
    std::fs::write(dir.join("EXPERIENCE.md"), format!("# task {id}\n")).unwrap();
    std::fs::write(
        dir.join("entry.toml"),
        format!(
            "entry_id = \"{id}\"\ndomain = \"{domain}\"\njob_id = \"evo_1\"\nharness_version = \"h1-x\"\n\
             mode = \"parallel\"\nk = 1\nconfidence = \"{confidence}\"\ncreated_at = \"{at}\"\n\
             pinned = false\nstatus = \"active\"\ntimes_shown = 0\ncue = \"cue {id}\"\nlesson = \"lesson {id}\"\n"
        ),
    )
    .unwrap();
}

fn tiers(home: &Path, session: &str) -> String {
    let db = rusqlite::Connection::open(profile(home).join("state.db")).unwrap();
    db.query_row(
        "SELECT tiers FROM session_prompts WHERE session_id = ?1",
        [session],
        |row| row.get(0),
    )
    .unwrap()
}

fn trajectory(home: &Path, session: &str) -> Vec<Value> {
    ok(&run(
        home,
        &["trajectory", "show", session, "--format", "jsonl"],
    ))
    .lines()
    .map(|l| serde_json::from_str(l).unwrap())
    .collect()
}

/// Scenarios: Low-confidence lesson excluded; Lesson adopted mid-session;
/// Injection observable.
#[test]
fn sessions_of_a_domain_start_with_its_good_lessons_frozen() {
    let home = setup("\n[experience]\nsession_lessons = true\n");
    plant(
        home.path(),
        "devops",
        "20261029-low-111111",
        "low",
        "2026-10-29T00:00:00Z",
    );
    plant(
        home.path(),
        "devops",
        "20261029-high-222222",
        "high",
        "2026-10-29T00:00:01Z",
    );
    plant(
        home.path(),
        "devops",
        "20261029-mid-333333",
        "medium",
        "2026-10-29T00:00:02Z",
    );
    ok(&run(
        home.path(),
        &["-z", "TASK-hello", "--domain", "DevOps"],
    ));
    let open = newest_session(home.path());
    let text = tiers(home.path(), &open);
    assert!(
        text.contains("## Lessons from earlier work in devops"),
        "{text}"
    );
    assert!(
        text.contains("cue 20261029-high-222222 → lesson 20261029-high-222222"),
        "{text}"
    );
    assert!(text.contains("cue 20261029-mid-333333"), "{text}");
    assert!(
        !text.contains("20261029-low-111111"),
        "low confidence stays out"
    );
    let injected: Vec<Value> = trajectory(home.path(), &open)
        .into_iter()
        .filter(|e| e["type"] == "experience.session_lessons.injected")
        .collect();
    assert_eq!(injected.len(), 1);
    assert_eq!(injected[0]["payload"]["session"], open.as_str());
    assert_eq!(
        injected[0]["payload"]["entry_ids"],
        serde_json::json!(["20261029-mid-333333", "20261029-high-222222"])
    );
    let shown = entries(home.path(), &["--domain", "devops"]);
    assert!(
        shown
            .iter()
            .filter(|e| e["confidence"] != "low")
            .all(|e| e["times_shown"] == 1),
        "{shown:?}"
    );

    // A lesson adopted while the session is open reaches only the next one.
    plant(
        home.path(),
        "devops",
        "20261030-new-444444",
        "high",
        "2026-10-30T00:00:00Z",
    );
    ok(&run(
        home.path(),
        &["chat", "-r", &open, "-z", "TASK-again"],
    ));
    assert_eq!(
        tiers(home.path(), &open),
        text,
        "the open session's prompt is unchanged"
    );
    ok(&run(
        home.path(),
        &["-z", "TASK-next", "--domain", "devops"],
    ));
    let next = newest_session(home.path());
    assert!(tiers(home.path(), &next).contains("cue 20261030-new-444444"));
    // Without a domain there is no layer.
    ok(&run(home.path(), &["-z", "TASK-plain"]));
    let plain = newest_session(home.path());
    assert!(!tiers(home.path(), &plain).contains("## Lessons from earlier work"));
}

/// `--domain` tags the session; its evolution job files under it.
#[test]
fn a_session_domain_tag_is_inherited_by_its_evolution_job() {
    let home = setup("");
    let out = run(home.path(), &["-z", "TASK-SUM tagged", "--domain", "ops"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let session = newest_session(home.path());
    let db = rusqlite::Connection::open(profile(home.path()).join("state.db")).unwrap();
    let tag: Option<String> = db
        .query_row(
            "SELECT domain FROM sessions WHERE id = ?1",
            [&session],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tag.as_deref(), Some("ops"));
    let queued = ok(&run(
        home.path(),
        &[
            "evolve",
            "run",
            "--session",
            &session,
            "--k",
            "1",
            "--detach",
        ],
    ));
    assert!(queued.contains("queued"), "{queued}");
    let jobs = json(home.path(), &["evolve", "list"]);
    assert_eq!(jobs[0]["domain"], "ops");
    let job = jobs[0]["id"].as_str().unwrap();
    let meta = std::fs::read_to_string(
        profile(home.path())
            .join("evolve/jobs")
            .join(job)
            .join("task/task.toml"),
    )
    .unwrap();
    assert!(meta.contains("domain_source = \"session\""), "{meta}");
}

/// `/domain` shows and sets the session's tag.
#[cfg(unix)]
#[test]
fn the_domain_slash_command_shows_and_sets_the_tag() {
    use common::pty::{exit, prompt, spawn};
    use expectrl::Expect as _;

    let home = setup("");
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("/domain").unwrap();
    p.expect("domain: none").unwrap();
    prompt(&mut p);
    p.send_line("/domain Finance / Ops").unwrap();
    p.expect("domain set to finance-ops").unwrap();
    prompt(&mut p);
    exit(p);
    let session = newest_session(home.path());
    let db = rusqlite::Connection::open(profile(home.path()).join("state.db")).unwrap();
    let tag: Option<String> = db
        .query_row(
            "SELECT domain FROM sessions WHERE id = ?1",
            [&session],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tag.as_deref(), Some("finance-ops"));
}

/// A kanban evolve task's `domain` files its job.
#[test]
fn a_kanban_evolve_task_files_its_job_under_the_task_domain() {
    let home = setup("");
    ok(&run(home.path(), &["-z", "TASK-SUM from a session"]));
    let session = newest_session(home.path());
    let created = json(
        home.path(),
        &[
            "kanban",
            "create",
            "evolve the ledger",
            "--assignee",
            "default",
            "--kind",
            "evolve",
            "--target-task",
            &session,
            "--budget-k",
            "1",
            "--domain",
            "Ledgers",
        ],
    );
    assert!(created["id"].is_string(), "{created}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let jobs = loop {
        ok(&run(home.path(), &["kanban", "dispatch", "--once"]));
        let jobs = json(home.path(), &["evolve", "list"]);
        if !jobs.as_array().unwrap().is_empty() {
            break jobs;
        }
        assert!(std::time::Instant::now() < deadline, "no job queued");
        std::thread::sleep(std::time::Duration::from_millis(300));
    };
    assert_eq!(jobs[0]["trigger"], "kanban");
    assert_eq!(jobs[0]["domain"], "ledgers");
}
