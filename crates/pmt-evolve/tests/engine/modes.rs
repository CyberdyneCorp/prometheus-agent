use pmt_config::schema::EvolveMode;
use pmt_evolve::engine::ledger::Kind;
use pmt_evolve::engine::ports::SessionKind;

use crate::support::{FakeImprover, FakeRollouts, Fixture, edit_guardrails, read_json};

/// Scenario: Parallel ledger.
#[test]
fn parallel_k3_ledgers_twelve_rollouts_and_nine_sessions() {
    let fx = Fixture::new(&[]);
    let job = fx.job(3, EvolveMode::Parallel);
    let rollouts = FakeRollouts::default();
    let improver = FakeImprover::default();
    let done = fx.run(&job, &rollouts, &improver).unwrap();
    assert_eq!(done.summary.status, "ok", "{:?}", done.summary);
    let ledger: pmt_evolve::engine::ledger::Ledger =
        serde_json::from_value(read_json(&fx.job_dir(&job).join("ledger.json"))).unwrap();
    let mode = &ledger.modes[0];
    let labels = |kind| mode.of(kind).map(|e| e.label.clone()).collect::<Vec<_>>();
    assert_eq!(
        labels(Kind::Rollout),
        [
            "base1", "base2", "base3", "inv1", "dom1", "inv2", "dom2", "inv3", "dom3", "agg1",
            "agg2", "agg3"
        ]
    );
    assert_eq!(
        labels(Kind::Session),
        [
            "inv1", "dom1", "inv2", "dom2", "inv3", "dom3", "agg1", "agg2", "agg3"
        ]
    );
    assert!(mode.selection.is_some(), "a selector ran at k=3");
    let similarity = done.summary.modes[0].similarity.unwrap();
    assert_eq!(similarity.n, 3);
    assert_eq!(rollouts.calls().len(), 12);
    // Every stage edited identical evidence: no pool, no decisions.
    for mission in improver
        .mission("inv1")
        .iter()
        .chain(&improver.mission("dom2"))
    {
        assert!(!mission.contains("context/pool/"), "{mission}");
    }
}

/// Scenario: Concurrency limits, out-of-order completion.
#[test]
fn ledger_order_is_serial_even_when_dom1_finishes_first() {
    let fx = Fixture::new(&[]);
    let job = fx.job(1, EvolveMode::Parallel);
    let mut rollouts = FakeRollouts::default();
    rollouts.delays.insert("inv1".into(), 300);
    let improver = FakeImprover::default();
    fx.run(&job, &rollouts, &improver).unwrap();
    let finished = rollouts.finished.lock().unwrap().clone();
    let at = |l: &str| finished.iter().position(|x| x == l).unwrap();
    assert!(at("dom1") < at("inv1"), "dom1 finished first: {finished:?}");
    let ledger = read_json(&fx.job_dir(&job).join("ledger.json"));
    let order: Vec<&str> = ledger["modes"][0]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "rollout")
        .map(|e| e["label"].as_str().unwrap())
        .collect();
    assert_eq!(order, ["base1", "inv1", "dom1", "agg1"]);
}

/// Scenario: Second round sees history.
#[test]
fn sequential_round_two_sees_the_first_checkpoint() {
    let fx = Fixture::new(&[]);
    let job = fx.job(2, EvolveMode::Sequential);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = std::sync::Arc::clone(&seen);
    let improver = FakeImprover::with(move |r, _| {
        if r.label == "inv2" {
            let context = r.workspace.join("context");
            record.lock().unwrap().push((
                context.join("pool/c1/rollout.md").is_file(),
                context.join("decisions/c1.md").is_file(),
                std::fs::read_to_string(context.join("harness/GUARDRAILS.md")).unwrap_or_default(),
            ));
        }
        Ok(edit_guardrails(r, &format!("- {}", r.label)))
    });
    let done = fx.run(&job, &FakeRollouts::default(), &improver).unwrap();
    assert_eq!(done.summary.status, "ok");
    let seen = seen.lock().unwrap();
    let (rollout, decision, harness) = &seen[0];
    assert!(*rollout && *decision, "inv2 evidence lacks c1");
    assert!(
        harness.contains("- c1"),
        "inv2 edits the checkpoint c1: {harness}"
    );
    let labels: Vec<String> = improver
        .labels()
        .into_iter()
        .filter(|l| !l.starts_with("select") && !l.starts_with("experience."))
        .collect();
    let at = |l: &str| labels.iter().position(|x| x == l).unwrap();
    assert_eq!(labels.len(), 6, "{labels:?}");
    assert!(at("inv1").max(at("dom1")) < at("c1") && at("c1") < at("inv2").min(at("dom2")));
    assert!(at("inv2").max(at("dom2")) < at("c2"), "{labels:?}");
}

/// Scenario: Pool diff baseline.
#[test]
fn a_composition_sees_each_candidate_diffed_against_the_active_harness() {
    let fx = Fixture::new(&[]);
    let job = fx.job(1, EvolveMode::Parallel);
    let diffs = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let keep = std::sync::Arc::clone(&diffs);
    let improver = FakeImprover::with(move |r, _| {
        if r.label == "agg1" {
            let pool = r.workspace.join("context/pool");
            *keep.lock().unwrap() =
                std::fs::read_to_string(pool.join("inv1/harness.diff")).unwrap();
            assert!(pool.join("dom1/produced.md").is_file());
            assert!(r.workspace.join("context/decisions/dom1.md").is_file());
            let harness =
                std::fs::read_to_string(r.workspace.join("context/harness/GUARDRAILS.md")).unwrap();
            assert!(
                harness.is_empty(),
                "a composition edits a fresh copy of the active harness"
            );
        }
        Ok(edit_guardrails(r, &format!("- rule from {}", r.label)))
    });
    fx.run(&job, &FakeRollouts::default(), &improver).unwrap();
    let diff = diffs.lock().unwrap().clone();
    assert!(diff.contains("--- a/base/GUARDRAILS.md"), "{diff}");
    assert!(diff.contains("+- rule from inv1"), "{diff}");
    assert!(!diff.contains("dom1"), "{diff}");
}

#[test]
fn no_ok_base_rollout_ends_the_job_base_failed() {
    let fx = Fixture::new(&[]);
    let job = fx.job(2, EvolveMode::Both);
    let rollouts =
        FakeRollouts::with(|_| pmt_evolve::engine::ports::RolloutOutcome::failed("model error"));
    let improver = FakeImprover::default();
    let done = fx.run(&job, &rollouts, &improver).unwrap();
    assert_eq!(done.summary.status, "base-failed");
    assert_eq!(
        done.summary.error.as_deref(),
        Some("no base rollout produced anything")
    );
    assert!(improver.labels().is_empty(), "no mode ran");
    assert_eq!(rollouts.calls(), ["base1", "base2"]);
}

#[test]
fn both_modes_share_the_base_rollouts_and_each_yield_a_proposal() {
    let fx = Fixture::new(&[]);
    let job = fx.job(1, EvolveMode::Both);
    let rollouts = FakeRollouts::default();
    let done = fx.run(&job, &rollouts, &FakeImprover::default()).unwrap();
    assert_eq!(done.summary.status, "ok");
    assert_eq!(
        rollouts.calls().iter().filter(|l| *l == "base1").count(),
        1,
        "shared"
    );
    assert_eq!(done.proposals.len(), 2);
    let ledger = read_json(&fx.job_dir(&job).join("ledger.json"));
    assert_eq!(
        ledger["modes"][1]["entries"][0]["cached"], true,
        "sequential reuses base1"
    );
    let _ = SessionKind::Select;
}
