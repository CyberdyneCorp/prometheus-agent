//! Skill evals and eval-gated edits through `skill_manage` with a gate
//! that scores the skill by its own text (spec: skills, Skill evals;
//! Eval-gated skill edits). The binary's gate, which runs the evals for
//! real, is covered by `crates/prometheus/tests/skills_evals.rs`.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use pmt_skills::evals::{self, Evals, Gate, Rate};
use serde_json::json;

use crate::support::{Profile, Session, add_skill, write};

const EVALS: &str = r#"
[[case]]
prompt = "say alpha"
expect_output_contains = "alpha"

[[case]]
prompt = "say beta"
expect_output_contains = "beta"

[[case]]
prompt = "say gamma"
expect_output_contains = "gamma"
"#;

/// A case passes when its word is in the skill's SKILL.md (a stand-in
/// for a model that follows the skill); remembers every rate it gave.
#[derive(Default)]
struct WordGate {
    rates: Mutex<Vec<Rate>>,
}

impl Gate for WordGate {
    fn rate(&self, dir: &Path, _name: &str, repeat: u32) -> Result<Option<Rate>, String> {
        let Some(evals) = Evals::load(dir)? else {
            return Ok(None);
        };
        let text = std::fs::read_to_string(dir.join("SKILL.md")).map_err(|e| e.to_string())?;
        let words = ["alpha", "beta", "gamma"];
        let passing = evals
            .cases
            .iter()
            .zip(words)
            .filter(|(_, w)| text.contains(w))
            .count();
        let cases = u32::try_from(evals.cases.len()).unwrap();
        let rate = Rate {
            passed: u32::try_from(passing).unwrap() * repeat,
            total: cases * repeat,
        };
        self.rates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(rate);
        Ok(Some(rate))
    }
}

fn foo(profile: &Profile) {
    add_skill(&profile.skills(), "foo", "foo", "Say words.");
    let dir = profile.skills().join("foo");
    let body = "---\nname: foo\ndescription: Say words.\n---\nSay alpha, beta or gamma.\n";
    write(&dir, "SKILL.md", body);
    write(&dir, "evals/evals.toml", EVALS);
}

async fn session(profile: &Profile, review: bool, gate: &Arc<WordGate>) -> Session {
    let mut setup = profile.setup(profile.session());
    setup.review = review;
    setup.eval_gate = Some(Arc::clone(gate) as Arc<dyn Gate>);
    let session = Session::start(profile, setup).await;
    session.json("skill_view", json!({"name": "foo"})).await;
    session
}

fn regressing_patch() -> serde_json::Value {
    json!({"action": "patch", "name": "foo", "old_string": "Say alpha, beta or gamma.", "new_string": "Say alpha."})
}

#[tokio::test]
async fn a_regressing_background_review_edit_is_rejected_and_the_skill_kept() {
    // Scenario: Regressing agent edit is rolled back.
    let profile = Profile::new("[skills]\neval_gate = true\n");
    foo(&profile);
    let gate = Arc::new(WordGate::default());
    let review = session(&profile, true, &gate).await;
    let refused = review.call("skill_manage", regressing_patch()).await;
    assert!(!refused.status.succeeded());
    let text = refused.text_content();
    assert!(
        text.contains("edit rejected: eval pass rate fell from 9/9 to 3/9"),
        "{text}"
    );
    let kept = std::fs::read_to_string(profile.skills().join("foo/SKILL.md")).unwrap();
    assert!(kept.contains("Say alpha, beta or gamma."), "{kept}");
    // Three runs per case by default (skills.eval_gate_repeat).
    assert_eq!(gate.rates.lock().unwrap().first().map(|r| r.total), Some(9));
}

#[tokio::test]
async fn a_user_session_edit_is_only_warned_and_a_gate_off_profile_never_runs_evals() {
    let profile = Profile::new("[skills]\neval_gate = true\neval_gate_repeat = 1\n");
    foo(&profile);
    let gate = Arc::new(WordGate::default());
    let user = session(&profile, false, &gate).await;
    let out = user.json("skill_manage", regressing_patch()).await;
    let message = out["message"].as_str().unwrap();
    assert!(
        message.contains("the edit lowered the evals: eval pass rate fell from 3/3 to 1/3"),
        "{message}"
    );
    let now = std::fs::read_to_string(profile.skills().join("foo/SKILL.md")).unwrap();
    assert!(now.contains("Say alpha.\n"), "{now}");

    let off = Profile::new("");
    foo(&off);
    let gate = Arc::new(WordGate::default());
    let review = session(&off, true, &gate).await;
    review.json("skill_manage", regressing_patch()).await;
    assert!(gate.rates.lock().unwrap().is_empty());
}

#[test]
fn an_evals_file_reads_its_cases_and_refuses_paths_outside_evals() {
    let profile = Profile::new("");
    foo(&profile);
    let dir = profile.skills().join("foo");
    let loaded = Evals::load(&dir).unwrap().unwrap();
    assert_eq!(loaded.cases.len(), 3);
    assert_eq!(loaded.cases[1].name, "case-2");
    assert!(evals::has_evals(&dir));
    write(
        &dir,
        "evals/evals.toml",
        "[[case]]\nprompt = \"x\"\nsetup_files = [\"../secret\"]\n",
    );
    assert!(
        Evals::load(&dir)
            .unwrap_err()
            .contains("not a path inside evals/")
    );
}

/// Merges `bar` into `foo` with a text that keeps only alpha.
struct MergeIntoFoo;

impl pmt_skills::curator::consolidate::Consolidator for MergeIntoFoo {
    fn propose(
        &self,
        _candidates: &[pmt_skills::curator::consolidate::Candidate],
    ) -> Result<Vec<pmt_skills::curator::consolidate::Merge>, pmt_core::error::Error> {
        Ok(vec![pmt_skills::curator::consolidate::Merge {
            into: "foo".to_owned(),
            absorbed: vec!["bar".to_owned()],
            skill_md: "---\nname: foo\ndescription: Say words.\n---\nSay alpha.\n".to_owned(),
        }])
    }
}

#[test]
fn a_regressing_curator_merge_is_refused_and_both_skills_stay() {
    use pmt_skills::curator::run::{self, Options};
    use pmt_store::skill_usage::{CreatedBy, SkillActivity};

    let profile = Profile::new("[skills]\neval_gate = true\neval_gate_repeat = 1\n");
    foo(&profile);
    add_skill(&profile.skills(), "bar", "bar", "Say more words.");
    for name in ["foo", "bar"] {
        profile
            .store
            .record_skill_activity(name, SkillActivity::Patched, CreatedBy::Agent)
            .unwrap();
    }
    let catalog = pmt_skills::eject::profile_catalog(
        &profile.scope,
        None,
        None,
        Arc::new(pmt_skills::bundled::Bundled::default()),
    );
    let cron = std::collections::BTreeSet::new();
    let gate = WordGate::default();
    let scope = pmt_skills::curator::Scope {
        profile_dir: profile.scope.dir(),
        store: &profile.store,
        config: profile.scope.config(),
        catalog: &catalog,
        cron_skills: &cron,
        now: jiff::Timestamp::now(),
        eval_gate: Some(&gate),
    };
    let options = Options {
        consolidate: true,
        ..Options::default()
    };
    let scanner = pmt_skills::scan::PatternScanner;
    let report = run::run(&scope, options, Some(&MergeIntoFoo), &scanner).unwrap();
    assert!(report.consolidated.is_empty());
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("edit rejected: eval pass rate fell from 3/3 to 1/3")),
        "{:?}",
        report.errors
    );
    let kept = std::fs::read_to_string(profile.skills().join("foo/SKILL.md")).unwrap();
    assert!(kept.contains("Say alpha, beta or gamma."), "{kept}");
    assert!(profile.skills().join("bar/SKILL.md").exists());
}
