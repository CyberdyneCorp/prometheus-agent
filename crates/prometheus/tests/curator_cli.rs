//! The curator through the binary (spec: skills, Curator CLI; Curator
//! transitions, scope and backups; Curator audit ledger, single-edit
//! rollback, purge and reports; /curator slash command).

mod common;

use std::path::Path;

use common::{ok, run, stderr, stdout, temp_home};

fn skills(home: &Path) -> std::path::PathBuf {
    home.join("profiles/default/skills")
}

fn skill(home: &Path, name: &str, body: &str) {
    let dir = skills(home).join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: Notes on {name}.\n---\n{body}\n"),
    )
    .unwrap();
}

/// Makes `name` an agent-created skill last used `days` ago.
fn agent_used(home: &Path, name: &str, days: i64) {
    ok(&run(home, &["curator", "status"]));
    let db = rusqlite::Connection::open(home.join("profiles/default/state.db")).unwrap();
    let at = jiff::Timestamp::now().as_millisecond() - days * 86_400_000;
    db.execute(
        "INSERT INTO skill_usage (name, created_by, last_activity_at) VALUES (?1, 'agent', ?2) \
         ON CONFLICT (name) DO UPDATE SET created_by = 'agent', last_activity_at = ?2",
        rusqlite::params![name, at],
    )
    .unwrap();
}

fn tree(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = pmt_skills::layout::walk(dir)
        .into_iter()
        .map(|rel| {
            let bytes = std::fs::read(dir.join(&rel)).unwrap();
            (rel, bytes)
        })
        .collect();
    out.sort();
    out
}

fn usage_rows(home: &Path) -> Vec<String> {
    let db = rusqlite::Connection::open(home.join("profiles/default/state.db")).unwrap();
    let mut stmt = db
        .prepare("SELECT name, state, last_activity_at, pinned FROM skill_usage ORDER BY name")
        .unwrap();
    stmt.query_map([], |r| {
        Ok(format!(
            "{}|{}|{:?}|{}",
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<i64>>(2)?,
            r.get::<_, i64>(3)?
        ))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

#[test]
fn dry_run_changes_nothing_and_prints_the_plan() {
    let home = temp_home();
    skill(home.path(), "old-notes", "Old.");
    agent_used(home.path(), "old-notes", 40);
    let before = (tree(&skills(home.path())), usage_rows(home.path()));
    let out = ok(&run(home.path(), &["curator", "run", "--dry-run"]));
    assert!(out.contains("old-notes: active -> archived"), "{out}");
    assert_eq!(
        (tree(&skills(home.path())), usage_rows(home.path())),
        before
    );
}

#[test]
fn every_real_pass_is_backed_up_and_purge_is_explicit() {
    let home = temp_home();
    skill(home.path(), "old-notes", "Old.");
    agent_used(home.path(), "old-notes", 40);
    let out = ok(&run(home.path(), &["curator", "run", "--sync"]));
    assert!(out.contains("1 archived"), "{out}");
    let backups: Vec<_> = std::fs::read_dir(skills(home.path()).join(".backups"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(backups.len(), 1);
    assert!(
        skills(home.path())
            .join(".archive/old-notes/SKILL.md")
            .exists()
    );
    let logs = home.path().join("profiles/default/logs/curator");
    let report = std::fs::read_dir(&logs)
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path();
    assert!(report.join("REPORT.md").exists() && report.join("run.json").exists());
    // A pass with a TTL never purges; the explicit purge does.
    common::append_config(home.path(), "\n[curator]\narchive_ttl_secs = 1\n");
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    ok(&run(home.path(), &["curator", "run", "--sync"]));
    assert!(skills(home.path()).join(".archive/old-notes").exists());
    let out = ok(&run(home.path(), &["curator", "purge"]));
    assert!(out.contains("deleted skills/.archive/old-notes"), "{out}");
    let ledger = ok(&run(home.path(), &["curator", "ledger"]));
    assert!(
        ledger.contains("purge") && ledger.contains("archive"),
        "{ledger}"
    );
}

#[test]
fn a_pass_that_cannot_read_the_scheduled_jobs_touches_nothing() {
    let home = temp_home();
    skill(home.path(), "old-notes", "Old.");
    agent_used(home.path(), "old-notes", 40);
    // A job might use any skill: without the jobs the pass must not run.
    let db = rusqlite::Connection::open(home.path().join("profiles/default/state.db")).unwrap();
    db.execute_batch("ALTER TABLE cron_jobs RENAME TO cron_jobs_unreadable")
        .unwrap();
    drop(db);
    let before = tree(&skills(home.path()));
    let out = run(home.path(), &["curator", "run", "--sync"]);
    assert_eq!(out.status.code(), Some(75), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("cannot read the scheduled jobs"),
        "{}",
        stderr(&out)
    );
    assert_eq!(tree(&skills(home.path())), before);
    assert!(!skills(home.path()).join(".archive").exists());
}

#[test]
fn rollback_undoes_one_entry_and_restore_brings_a_skill_back() {
    let home = temp_home();
    skill(home.path(), "deploy", "Ship by hand.");
    ok(&run(home.path(), &["curator", "archive", "deploy"]));
    assert!(!skills(home.path()).join("deploy").exists());
    let ledger = ok(&run(
        home.path(),
        &["curator", "ledger", "--skill", "deploy"],
    ));
    let label = ledger.split_whitespace().next().unwrap().to_owned();
    let out = ok(&run(home.path(), &["curator", "rollback", &label]));
    assert!(out.contains(&format!("Rolled back {label}")), "{out}");
    assert!(skills(home.path()).join("deploy/SKILL.md").exists());
    assert!(!skills(home.path()).join(".archive/deploy").exists());
    let ledger = ok(&run(home.path(), &["curator", "ledger"]));
    assert!(
        ledger.lines().next().unwrap().contains("rollback"),
        "{ledger}"
    );
    let out = run(home.path(), &["curator", "rollback", "L999"]);
    assert_eq!(out.status.code(), Some(75), "{}", stderr(&out));
    ok(&run(home.path(), &["curator", "archive", "deploy"]));
    ok(&run(home.path(), &["curator", "restore", "deploy"]));
    assert!(skills(home.path()).join("deploy/SKILL.md").exists());
}

#[test]
fn pin_adopt_pause_and_listings() {
    let home = temp_home();
    skill(home.path(), "mine", "Mine.");
    let unmanaged = ok(&run(home.path(), &["curator", "list-unmanaged"]));
    assert!(unmanaged.contains("mine"), "{unmanaged}");
    ok(&run(home.path(), &["curator", "adopt", "mine"]));
    assert!(!ok(&run(home.path(), &["curator", "list-unmanaged"])).contains("mine"));
    ok(&run(home.path(), &["curator", "pin", "mine"]));
    ok(&run(home.path(), &["curator", "pause"]));
    let status = ok(&run(home.path(), &["curator", "status"]));
    assert!(
        status.contains("curator: paused") && status.contains("pinned: mine"),
        "{status}"
    );
    let out = run(home.path(), &["curator", "archive", "mine"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("pinned"),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
}

#[cfg(feature = "gateway")]
#[test]
fn pin_from_a_messaging_chat() {
    use common::gateway::{Daemon, gateway_home, send, wait_said};
    let home = gateway_home("");
    skill(home.path(), "deploy-runbook", "Deploy.");
    let daemon = Daemon::start(home.path());
    send(
        home.path(),
        "default",
        "alice",
        "/curator pin deploy-runbook",
    );
    wait_said(home.path(), "default", "Pinned 'deploy-runbook'");
    daemon.kill();
    let rows = usage_rows(home.path());
    assert!(
        rows.iter()
            .any(|r| r.starts_with("deploy-runbook|") && r.ends_with("|1")),
        "{rows:?}"
    );
}

/// The review pass runs on the `auxiliary.curator` slot (spec: skills,
/// Curator audit ledger, single-edit rollback, purge and reports): the
/// merge it answers is applied, and its call is accounted to that slot's
/// model in a hidden curator session.
#[test]
fn consolidation_asks_the_auxiliary_curator_model() {
    let home = common::mock_home();
    skill(home.path(), "deploy-web", "Ship the web app.");
    skill(home.path(), "deploy-api", "Ship the API.");
    agent_used(home.path(), "deploy-web", 1);
    agent_used(home.path(), "deploy-api", 1);
    let merged =
        "---\nname: deploy\ndescription: Ship any service.\n---\nShip the web app and the API.\n";
    let reply = serde_json::json!({"merges": [{
        "into": "deploy",
        "absorbed": ["deploy-web", "deploy-api"],
        "skill_md": merged,
    }]})
    .to_string();
    std::fs::write(
        home.path().join("profiles/default/mock.toml"),
        format!("[[rule]]\nrepeat = true\ntext = {reply:?}\n"),
    )
    .unwrap();
    common::append_config(
        home.path(),
        "\n[providers.mock]\nscript = \"mock.toml\"\n\n[auxiliary.curator]\nprovider = \"mock\"\nmodel = \"curator-reviewer\"\n",
    );
    let out = ok(&run(
        home.path(),
        &["curator", "run", "--sync", "--consolidate"],
    ));
    assert!(out.contains("deploy"), "{out}");
    assert_eq!(
        std::fs::read_to_string(skills(home.path()).join("deploy/SKILL.md")).unwrap(),
        merged
    );
    assert!(skills(home.path()).join(".archive/deploy-web").exists());
    assert!(skills(home.path()).join(".archive/deploy-api").exists());
    let db = rusqlite::Connection::open(home.path().join("profiles/default/state.db")).unwrap();
    let calls: Vec<(String, String, String, i64)> = db
        .prepare(
            "SELECT u.purpose, u.model, s.source_tag, s.hidden FROM usage_ledger u \
             JOIN sessions s ON s.id = u.session_id",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        calls,
        [(
            "auxiliary.curator".to_owned(),
            "curator-reviewer".to_owned(),
            "curator".to_owned(),
            1
        )]
    );
}
