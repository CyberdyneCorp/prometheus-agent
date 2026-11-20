use std::sync::Arc;

use pmt_config::schema::EvolveMode;
use pmt_evolve::engine::gate::Limits;
use pmt_evolve::engine::ports::{ImproverFailure, SessionKind};

use crate::support::{FakeImprover, FakeRollouts, Fixture, edit_guardrails, runtime};

/// Scenario: Cap reached mid-job.
#[test]
fn reaching_the_cap_during_agg1_stops_the_job_without_a_proposal() {
    let fx = Fixture::new(&[]);
    // Base 2 x 0.01 + stages 4 x (0.02 + 0.01) = 0.14; agg1's session
    // brings the spend to the cap.
    let job = fx.job_with(2, EvolveMode::Parallel, Some(0.16), &[]);
    let rollouts = FakeRollouts::default();
    let improver = FakeImprover::default();
    let done = fx
        .run_limited(&job, &rollouts, &improver, Arc::new(Limits::new(1, 1)))
        .unwrap();
    assert_eq!(done.summary.status, "budget-exceeded");
    assert!(done.proposals.is_empty());
    let labels = improver.labels();
    assert!(labels.contains(&"agg1".to_owned()), "{labels:?}");
    assert!(
        !labels
            .iter()
            .any(|l| l == "agg2" || l.starts_with("select")),
        "nothing starts after the cap: {labels:?}"
    );
    let costs = done.summary.costs;
    assert!(costs.total() >= 0.16 - 1e-9, "{costs:?}");
    assert!((costs.rollout - 0.06).abs() < 1e-9, "{costs:?}");
    assert!(
        fx.job_dir(&job).join("ledger.json").is_file(),
        "completed artifacts are kept"
    );
    assert_eq!(
        pmt_evolve::job::Jobs::of(fx.dir())
            .get(&job)
            .unwrap()
            .status,
        "budget-exceeded"
    );
}

/// Scenario: Cancel.
#[test]
fn cancel_during_inv2_stops_new_model_calls_and_records_spend() {
    let fx = Fixture::new(&[]);
    let job = fx.job(2, EvolveMode::Parallel);
    let cancel = fx.job_dir(&job).join(pmt_evolve::engine::CANCEL_FILE);
    let improver = FakeImprover::with(move |r, _| {
        if r.label == "inv2" {
            std::fs::write(&cancel, "").unwrap();
            let token = r.cancel.clone();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !token.is_cancelled() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            return Err(ImproverFailure {
                transport: false,
                message: "interrupted".into(),
                usd: 0.005,
            });
        }
        Ok(edit_guardrails(r, "- ok"))
    });
    let rollouts = FakeRollouts::default();
    let done = fx
        .run_limited(&job, &rollouts, &improver, Arc::new(Limits::new(1, 1)))
        .unwrap();
    assert_eq!(done.summary.status, "cancelled");
    let labels = improver.labels();
    assert!(
        !labels
            .iter()
            .any(|l| l == "dom2" || l.starts_with("agg") || l.starts_with("select")),
        "{labels:?}"
    );
    assert!(done.summary.costs.total() > 0.0);
    assert!(
        !fx.job_dir(&job).join("tmp").exists(),
        "sandboxes are removed"
    );
}

/// Scenario: Concurrency limits (sessions bounded across jobs; one job
/// per profile and domain at a time).
#[test]
fn sessions_stay_within_the_process_bound_and_one_domain_runs_one_job() {
    let fx = Fixture::new(&[]);
    let first = fx.job(2, EvolveMode::Parallel);
    let second = fx.job_with(
        2,
        EvolveMode::Parallel,
        None,
        &[("notes.txt", "a different task")],
    );
    let rollouts = FakeRollouts::default();
    let improver = FakeImprover::default();
    let limits = Arc::new(Limits::new(2, 2));
    let rules = Fixture::rules();
    let config = &fx.scope.config().evolve;
    let engine = pmt_evolve::engine::Engine {
        profile_dir: fx.dir(),
        config,
        rules: &rules,
        rollouts: &rollouts,
        improver: &improver,
        limits,
        retry_delay: std::time::Duration::ZERO,
        experience: &fx.scope.config().experience,
        estimate_usd: 0.0,
    };
    let (a, b) =
        runtime().block_on(async { tokio::join!(engine.run(&first), engine.run(&second)) });
    assert_eq!(a.unwrap().summary.status, "ok");
    assert_eq!(b.unwrap().summary.status, "ok");
    assert!(improver.peak() <= 2, "peak {}", improver.peak());
    // FIFO per domain: every improver session of one job precedes the
    // other's (the default improver only edits; selectors carry the id).
    let calls = improver.calls.lock().unwrap().clone();
    let selects: Vec<usize> = calls
        .iter()
        .enumerate()
        .filter(|(_, (l, _))| l.starts_with("select"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(selects.len(), 2);
    // Each job ends with its selection then its reflection.
    assert!(
        calls[..selects[0]].len() == 6
            && calls[selects[0] + 1].0 == "experience.parallel2"
            && calls[selects[0] + 2..selects[1]].len() == 6,
        "jobs interleaved: {calls:?}"
    );
    let _ = SessionKind::Select;
}
