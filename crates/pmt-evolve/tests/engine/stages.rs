use pmt_config::schema::EvolveMode;
use pmt_evolve::engine::ports::{ImproverFailure, ImproverReply};
use pmt_evolve::engine::prompts::{ANSWER_FREE, EVIDENCE_BOUNDARY, PROMPT_VERSION, REJECTED};

use crate::support::{FakeImprover, FakeRollouts, Fixture, edit_guardrails, read_json};

fn stage<'a>(
    summary: &'a pmt_evolve::engine::summary::JobSummary,
    label: &str,
) -> &'a pmt_evolve::engine::summary::StageSummary {
    summary.modes[0]
        .stages
        .iter()
        .find(|s| s.label == label)
        .unwrap()
}

/// Scenario: Invalid then valid.
#[test]
fn a_rejected_script_is_retried_from_the_models_own_edit() {
    let fx = Fixture::new(&[]);
    let job = fx.job(1, EvolveMode::Parallel);
    let improver = FakeImprover::with(|r, seen| {
        if r.label != "inv1" {
            return Ok(edit_guardrails(r, "- ok"));
        }
        let script = r.workspace.join("context/harness/scripts/fix.py");
        if seen == 0 {
            std::fs::create_dir_all(script.parent().unwrap()).unwrap();
            std::fs::write(&script, "def broken(:\n").unwrap();
        } else {
            assert!(script.is_file(), "attempt 2 starts from the rejected edit");
            std::fs::write(&script, "def fixed():\n    return 1\n").unwrap();
        }
        Ok(ImproverReply {
            text: "wrote scripts/fix.py (rollouts/base1.md step 3)".into(),
            usd: 0.0,
        })
    });
    let done = fx.run(&job, &FakeRollouts::default(), &improver).unwrap();
    let inv1 = stage(&done.summary, "inv1");
    assert!(!inv1.fell_back, "{inv1:?}");
    assert_eq!(inv1.attempts, 2);
    assert!(
        (inv1.usd - 0.04).abs() < 1e-9,
        "both attempts are charged: {}",
        inv1.usd
    );
    let missions = improver.mission("inv1");
    assert!(!missions[0].contains(REJECTED));
    assert!(
        missions[1].contains(REJECTED) && missions[1].contains("scripts/fix.py"),
        "{}",
        missions[1]
    );
    assert!(!inv1.kept_base);
}

#[test]
fn two_rejections_fall_back_to_the_active_harness_and_are_still_rolled_out() {
    let fx = Fixture::new(&[]);
    let job = fx.job(1, EvolveMode::Parallel);
    let improver = FakeImprover::with(|r, _| {
        if r.label == "dom1" {
            std::fs::write(r.workspace.join("context/harness/notes.txt"), "off surface").unwrap();
            return Ok(ImproverReply {
                text: "added notes".into(),
                usd: 0.0,
            });
        }
        Ok(edit_guardrails(r, "- ok"))
    });
    let rollouts = FakeRollouts::default();
    let done = fx.run(&job, &rollouts, &improver).unwrap();
    let dom1 = stage(&done.summary, "dom1");
    assert!(dom1.fell_back && dom1.kept_base);
    assert_eq!(dom1.attempts, 2);
    assert!(
        dom1.error
            .as_deref()
            .unwrap()
            .contains("off-surface paths: notes.txt"),
        "{dom1:?}"
    );
    assert!(dom1.error.as_deref().unwrap().chars().count() <= 400);
    assert!(
        rollouts.calls().contains(&"dom1".to_owned()),
        "a fallen-back stage is rolled out"
    );
}

/// Improver sessions: a transport failure is retried once; exhaustion
/// falls back without a stage retry, its cost recorded.
#[test]
fn transport_failures_are_retried_once_then_fall_back_without_a_stage_retry() {
    let fx = Fixture::new(&[]);
    let job = fx.job(1, EvolveMode::Parallel);
    let improver = FakeImprover::with(|r, seen| match (r.label, seen) {
        ("inv1", 0) | ("dom1", _) => Err(ImproverFailure {
            transport: true,
            message: "connection reset".into(),
            usd: 0.01,
        }),
        _ => Ok(edit_guardrails(r, "- ok")),
    });
    let done = fx.run(&job, &FakeRollouts::default(), &improver).unwrap();
    let inv1 = stage(&done.summary, "inv1");
    assert!(!inv1.fell_back);
    assert_eq!(improver.mission("inv1").len(), 2, "one retry");
    let dom1 = stage(&done.summary, "dom1");
    assert!(dom1.fell_back);
    assert_eq!(
        dom1.attempts, 1,
        "no stage retry after transport exhaustion"
    );
    assert_eq!(improver.mission("dom1").len(), 2);
    assert!((dom1.usd - 0.02).abs() < 1e-9);
    assert_eq!(dom1.error.as_deref(), Some("connection reset"));
}

/// Scenario: Propose prompt blocks (rendered mission and session meta).
#[test]
fn every_mission_carries_the_blocks_and_the_cache_records_the_prompt_version() {
    let fx = Fixture::new(&[]);
    let job = fx.job(1, EvolveMode::Parallel);
    let improver = FakeImprover::default();
    fx.run(&job, &FakeRollouts::default(), &improver).unwrap();
    for (_, mission) in improver.calls.lock().unwrap().iter() {
        assert!(
            mission.contains(ANSWER_FREE) && mission.contains(EVIDENCE_BOUNDARY),
            "{mission}"
        );
    }
    let sessions = fx.dir().join("evolve/cache/sessions");
    let metas: Vec<_> = std::fs::read_dir(&sessions)
        .unwrap()
        .map(|e| read_json(&e.unwrap().path().join("meta.json")))
        .collect();
    assert!(!metas.is_empty());
    assert!(metas.iter().all(|m| m["prompt_version"] == PROMPT_VERSION));
    let dom1 = metas
        .iter()
        .find(|m| m["label"] == "dom1")
        .expect("dom1 cached");
    assert_eq!(dom1["model"], "mock/improver");
    assert!(
        !fx.job_dir(&job).join("tmp").exists(),
        "temporary directories are removed"
    );
}

/// Scenario: Eval verdict in source (found later in an evidence tree).
#[test]
fn a_verdict_in_a_rollout_deliverable_trips_every_stage_that_would_see_it() {
    let fx = Fixture::new(&[]);
    let job = fx.job(1, EvolveMode::Parallel);
    let rollouts = FakeRollouts::with(|r| {
        let mut out = crate::support::ok_rollout(r);
        out.diff = "+++ b/results/run.json\n+{\"suite\": \"smoke\", \"pass_rate\": 1.0}\n".into();
        out
    });
    let improver = FakeImprover::default();
    let done = fx.run(&job, &rollouts, &improver).unwrap();
    let inv1 = stage(&done.summary, "inv1");
    assert!(inv1.fell_back);
    assert!(
        inv1.error
            .as_deref()
            .unwrap()
            .starts_with("tripwire: verdict marker"),
        "{inv1:?}"
    );
    assert!(improver.mission("inv1").is_empty(), "no improver saw it");
}

/// 69.5 verification: a rubric planted in the frozen task aborts the job.
#[test]
fn a_planted_rubric_file_aborts_the_job() {
    let fx = Fixture::new(&[]);
    let job = fx.job(1, EvolveMode::Parallel);
    let planted = fx.job_dir(&job).join("task/source/grader/rubric.md");
    std::fs::create_dir_all(planted.parent().unwrap()).unwrap();
    std::fs::write(&planted, "full marks when the total is 7.50").unwrap();
    let rollouts = FakeRollouts::default();
    let improver = FakeImprover::default();
    let done = fx.run(&job, &rollouts, &improver).unwrap();
    assert_eq!(done.summary.status, "failed");
    assert!(
        done.summary
            .error
            .as_deref()
            .unwrap()
            .contains("grader/rubric.md matches a leak glob"),
        "{:?}",
        done.summary.error
    );
    assert!(
        rollouts.calls().is_empty() && improver.labels().is_empty(),
        "nothing ran"
    );
    assert!(done.proposals.is_empty());
}
