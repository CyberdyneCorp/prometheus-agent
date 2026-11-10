//! The curator: scope, transitions, backups, dry runs, purge, the ledger
//! and single-edit rollback (spec: skills, Curator configuration and
//! scheduling; Curator transitions, scope and backups; Curator CLI;
//! Curator audit ledger, single-edit rollback, purge and reports).

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use jiff::{SignedDuration, Timestamp};
use pmt_skills::bundled::Bundled;
use pmt_skills::curator::run::{self, Options};
use pmt_skills::curator::schedule::{self, Due};
use pmt_skills::curator::{self, Scope, moves};
use pmt_skills::ledger::{Actor, Evidence, Ledger};
use pmt_skills::lockfile::{Entry, LockFile, Trust};
use pmt_skills::scan::PatternScanner;
use pmt_skills::{Catalog, eject};
use pmt_store::skill_usage::{CreatedBy, SkillActivity, SkillState};
use serde_json::json;

use crate::support::{Profile, Session, add_skill};

fn days(n: i64) -> Timestamp {
    Timestamp::now()
        .checked_add(SignedDuration::from_hours(24 * n))
        .unwrap()
}

fn catalog(profile: &Profile) -> Catalog {
    eject::profile_catalog(&profile.scope, None, None, Arc::new(Bundled::default()))
}

fn agent_skill(profile: &Profile, name: &str) {
    add_skill(&profile.skills(), name, name, "Agent notes.");
    profile
        .store
        .record_skill_activity(name, SkillActivity::Patched, CreatedBy::Agent)
        .unwrap();
}

fn pass(
    profile: &Profile,
    now: Timestamp,
    cron: &BTreeSet<String>,
    dry_run: bool,
) -> pmt_skills::curator::report::RunReport {
    let catalog = catalog(profile);
    let scope = Scope {
        profile_dir: profile.scope.dir(),
        store: &profile.store,
        config: profile.scope.config(),
        catalog: &catalog,
        cron_skills: cron,
        now,
    };
    let options = Options {
        dry_run,
        ..Options::default()
    };
    run::run(&scope, options, None, &PatternScanner).unwrap()
}

/// Every file below `dir` with its bytes.
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

#[test]
fn every_real_pass_is_backed_up_and_only_managed_skills_move() {
    let profile = Profile::new("");
    agent_skill(&profile, "old-notes");
    agent_skill(&profile, "pinned-notes");
    profile
        .store
        .set_skill_pinned("pinned-notes", true)
        .unwrap();
    agent_skill(&profile, "job-notes");
    add_skill(&profile.skills(), "mine", "mine", "User notes.");
    add_skill(&profile.skills(), "hubbed", "hubbed", "Hub notes.");
    profile
        .store
        .record_skill_activity("hubbed", SkillActivity::Patched, CreatedBy::Agent)
        .unwrap();
    LockFile::in_dir(&profile.skills())
        .upsert(Entry {
            name: "hubbed".into(),
            source: "o/r/hubbed".into(),
            content_digest: pmt_skills::digest::dir(&profile.skills().join("hubbed")),
            trust: Some(Trust::Community),
            installed_at: "2026-01-01T00:00:00Z".into(),
            ..Entry::default()
        })
        .unwrap();
    let cron = BTreeSet::from(["job-notes".to_owned()]);
    let report = pass(&profile, days(31), &cron, false);
    let archived: Vec<&str> = report.archived.keys().map(String::as_str).collect();
    assert_eq!(archived, ["old-notes"]);
    assert!(
        profile
            .skills()
            .join(".archive/old-notes/SKILL.md")
            .exists()
    );
    for kept in ["pinned-notes", "job-notes", "mine", "hubbed"] {
        assert!(
            profile.skills().join(kept).join("SKILL.md").exists(),
            "{kept}"
        );
    }
    // The backup was taken before the move: it holds the skill in place.
    let backup = report.backup.unwrap();
    assert!(backup.starts_with(profile.skills().join(".backups")));
    let tarball = zstd::decode_all(std::fs::read(&backup).unwrap().as_slice()).unwrap();
    let mut archive = tar::Archive::new(tarball.as_slice());
    let names: Vec<String> = archive
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.contains(&"old-notes/SKILL.md".to_owned()),
        "{names:?}"
    );
    let state = profile
        .store
        .skill_usage("old-notes")
        .unwrap()
        .unwrap()
        .state;
    assert_eq!(state, SkillState::Archived);
    let report_dir = report.report_dir.unwrap();
    assert!(report_dir.join("run.json").exists() && report_dir.join("REPORT.md").exists());
    assert!(report_dir.starts_with(profile.scope.dir().join("logs/curator")));
}

#[test]
fn dry_run_changes_nothing() {
    let profile = Profile::new("");
    agent_skill(&profile, "old-notes");
    agent_skill(&profile, "quiet-notes");
    let skills_before = tree(&profile.skills());
    let usage_before = profile.store.skill_usages().unwrap();
    let report = pass(&profile, days(20), &BTreeSet::new(), true);
    assert_eq!(report.transitions.len(), 2, "{report:?}");
    assert!(report.transitions.iter().all(|t| t.to == "stale"));
    assert_eq!(tree(&profile.skills()), skills_before);
    assert_eq!(profile.store.skill_usages().unwrap(), usage_before);
    assert!(report.backup.is_none());
}

#[test]
fn stale_skills_go_stale_and_come_back_when_used() {
    let profile = Profile::new("");
    agent_skill(&profile, "notes");
    let report = pass(&profile, days(15), &BTreeSet::new(), false);
    assert_eq!(report.transitions[0].to, "stale");
    assert_eq!(
        profile.store.skill_usage("notes").unwrap().unwrap().state,
        SkillState::Stale
    );
    profile
        .store
        .record_skill_activity("notes", SkillActivity::Used, CreatedBy::Agent)
        .unwrap();
    let report = pass(&profile, Timestamp::now(), &BTreeSet::new(), false);
    assert_eq!(report.transitions[0].to, "active");
}

#[test]
fn purge_is_never_automatic() {
    let profile = Profile::new("[curator]\narchive_ttl_secs = 1\n");
    agent_skill(&profile, "old-notes");
    let ledger = Ledger::new(&profile.store, &profile.skills(), 0);
    moves::archive(
        &ledger,
        &profile.store,
        "old-notes",
        "old-notes",
        Actor::User,
        &Evidence::default(),
    )
    .unwrap();
    pass(&profile, days(400), &BTreeSet::new(), false);
    assert!(
        profile
            .skills()
            .join(".archive/old-notes/SKILL.md")
            .exists()
    );
    let purged = moves::purge(&ledger, 1, days(2)).unwrap();
    assert_eq!(purged.len(), 1);
    assert!(!profile.skills().join(".archive/old-notes").exists());
    let entries = profile.store.skill_ledger(Some("old-notes"), 0).unwrap();
    assert_eq!(entries[0].action, "purge");
    assert_eq!(entries[1].action, "archive");
}

#[test]
fn a_restore_refuses_a_name_now_taken_by_a_bundled_skill() {
    let profile = Profile::new("");
    agent_skill(&profile, "deploy");
    let ledger = Ledger::new(&profile.store, &profile.skills(), 0);
    moves::archive(
        &ledger,
        &profile.store,
        "deploy",
        "deploy",
        Actor::User,
        &Evidence::default(),
    )
    .unwrap();
    let bundled = Arc::new(Bundled::from_files([(
        "devops/deploy/SKILL.md",
        "---\nname: deploy\ndescription: Shipped.\n---\nbody\n",
    )]));
    let taken = eject::profile_catalog(&profile.scope, None, None, bundled);
    let err = moves::restore(&ledger, &profile.store, &taken, "deploy", Actor::User).unwrap_err();
    assert!(
        err.message().contains("a bundled skill"),
        "{}",
        err.message()
    );
    moves::restore(
        &ledger,
        &profile.store,
        &catalog(&profile),
        "deploy",
        Actor::User,
    )
    .unwrap();
    assert!(profile.skills().join("deploy/SKILL.md").exists());
}

#[test]
fn one_pass_at_a_time_and_the_first_observation_only_seeds() {
    let profile = Profile::new("");
    let config = profile.scope.config().curator.clone();
    assert_eq!(
        schedule::due(&profile.store, &config, Timestamp::now(), None).unwrap(),
        Due::Seeded
    );
    assert!(matches!(
        schedule::due(&profile.store, &config, days(1), None).unwrap(),
        Due::Later(_)
    ));
    assert_eq!(
        schedule::due(&profile.store, &config, days(8), None).unwrap(),
        Due::Now
    );
    let recent = Some(days(8));
    assert!(matches!(
        schedule::due(&profile.store, &config, days(8), recent).unwrap(),
        Due::Later(_)
    ));
    profile.store.set_curator_paused(true).unwrap();
    assert!(matches!(
        schedule::due(&profile.store, &config, days(9), None).unwrap(),
        Due::Later(_)
    ));

    let catalog = catalog(&profile);
    let cron = BTreeSet::new();
    let scope = Scope {
        profile_dir: profile.scope.dir(),
        store: &profile.store,
        config: profile.scope.config(),
        catalog: &catalog,
        cron_skills: &cron,
        now: Timestamp::now(),
    };
    let _held = run::lock(&scope).unwrap();
    let err = run::run(&scope, Options::default(), None, &PatternScanner).unwrap_err();
    assert_eq!(err.code().code, "curator.busy");
    let _ = curator::plan(&scope);
}

#[tokio::test]
async fn undo_one_agent_edit() {
    let profile = Profile::new("");
    add_skill(&profile.skills(), "deploy", "deploy", "Deploy notes.");
    add_skill(&profile.skills(), "other", "other", "Other notes.");
    let session = Session::open(&profile).await;
    let before = std::fs::read(profile.skills().join("deploy/SKILL.md")).unwrap();
    session
        .json("skill_manage", json!({"action": "patch", "name": "deploy", "old_string": "Deploy notes.", "new_string": "Deploy by hand."}))
        .await;
    session
        .json("skill_manage", json!({"action": "write_file", "name": "deploy", "file_path": "references/steps.md", "file_content": "1. ship"}))
        .await;
    session
        .json("skill_manage", json!({"action": "patch", "name": "other", "old_string": "Other notes.", "new_string": "Other, edited."}))
        .await;
    let entries = profile.store.skill_ledger(Some("deploy"), 0).unwrap();
    let patch = entries.iter().find(|e| e.action == "patch").unwrap();
    assert_eq!(patch.actor, "agent");
    assert_eq!(patch.files[0].path, "deploy/SKILL.md");
    let other_before = tree(&profile.skills().join("other"));
    let steps = profile.skills().join("deploy/references/steps.md");
    let ledger = Ledger::new(&profile.store, &profile.skills(), 0);
    ledger.rollback(patch.id).unwrap();
    assert_eq!(
        std::fs::read(profile.skills().join("deploy/SKILL.md")).unwrap(),
        before
    );
    assert_eq!(
        std::fs::read_to_string(&steps).unwrap(),
        "1. ship",
        "only the files of that entry"
    );
    assert_eq!(tree(&profile.skills().join("other")), other_before);
    let newest = &profile.store.skill_ledger(None, 1).unwrap()[0];
    assert_eq!(newest.action, "rollback");
    // Rolling back the write removes the file it created.
    let write = entries.iter().find(|e| e.action == "write_file").unwrap();
    ledger.rollback(write.id).unwrap();
    assert!(!steps.exists());
}

/// A review that merges `a` and `b` into `ab`.
struct MergeAb;

impl pmt_skills::curator::consolidate::Consolidator for MergeAb {
    fn propose(
        &self,
        candidates: &[pmt_skills::curator::consolidate::Candidate],
    ) -> Result<Vec<pmt_skills::curator::consolidate::Merge>, pmt_core::error::Error> {
        assert_eq!(candidates.len(), 2);
        Ok(vec![pmt_skills::curator::consolidate::Merge {
            into: "ab".to_owned(),
            absorbed: vec!["a".to_owned(), "b".to_owned()],
            skill_md: "---\nname: ab\ndescription: Both notes.\n---\nRules of a and b.\n"
                .to_owned(),
        }])
    }
}

#[test]
fn consolidation_merges_and_reports_the_rename_map() {
    let profile = Profile::new("");
    agent_skill(&profile, "a");
    agent_skill(&profile, "b");
    let catalog = catalog(&profile);
    let cron = BTreeSet::new();
    let scope = Scope {
        profile_dir: profile.scope.dir(),
        store: &profile.store,
        config: profile.scope.config(),
        catalog: &catalog,
        cron_skills: &cron,
        now: Timestamp::now(),
    };
    let options = Options {
        consolidate: true,
        ..Options::default()
    };
    let report = run::run(&scope, options, Some(&MergeAb), &PatternScanner).unwrap();
    assert_eq!(report.rename_map.get("a").map(String::as_str), Some("ab"));
    assert_eq!(report.rename_map.get("b").map(String::as_str), Some("ab"));
    assert!(profile.skills().join("ab/SKILL.md").exists());
    assert!(!profile.skills().join("a").exists() && profile.skills().join(".archive/a").exists());
    let markdown = std::fs::read_to_string(report.report_dir.unwrap().join("REPORT.md")).unwrap();
    assert!(markdown.contains("a → ab"), "{markdown}");
    let archived = profile.store.skill_ledger(Some("a"), 1).unwrap();
    assert_eq!(archived[0].absorbed_into.as_deref(), Some("ab"));
}
