use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pmt_config::schema::EvolveMode;
use pmt_evolve::engine::gate::Limits;
use pmt_evolve::engine::ports::ImproverFailure;
use pmt_evolve::job::Jobs;

use crate::support::{FakeImprover, FakeRollouts, Fixture, edit_guardrails};

/// Scenario: Resume after crash.
#[test]
fn a_job_stopped_after_agg1_resumes_with_only_agg2_and_the_selection() {
    let fx = Fixture::new(&[]);
    let job = fx.job(2, EvolveMode::Parallel);
    let rollouts = Arc::new(FakeRollouts::default());
    let dir = fx.job_dir(&job);
    let cancel = dir.join(pmt_evolve::engine::CANCEL_FILE);
    let agg1 = dir.join("modes/parallel/agg1/rollout.json");
    let crashing = FakeImprover::with(move |r, _| {
        if r.label == "agg2" {
            // The crash: after agg1 finished (its rollout recorded).
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !agg1.is_file() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            std::fs::write(&cancel, "").unwrap();
            while !r.cancel.is_cancelled() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            return Err(ImproverFailure {
                transport: false,
                message: "killed".into(),
                usd: 0.0,
            });
        }
        Ok(edit_guardrails(r, &format!("- {}", r.label)))
    });
    let stopped = fx
        .run_limited(&job, &rollouts, &crashing, Arc::new(Limits::new(1, 1)))
        .unwrap();
    assert_eq!(stopped.summary.status, "cancelled");
    Jobs::of(fx.dir()).resume(&job).unwrap();
    let rollouts = FakeRollouts::default();
    let improver = FakeImprover::with(|r, _| Ok(edit_guardrails(r, &format!("- {}", r.label))));
    let done = fx.run(&job, &rollouts, &improver).unwrap();
    assert_eq!(done.summary.status, "ok");
    assert_eq!(
        improver.labels(),
        ["agg2", "select-parallel", "experience.parallel2"]
    );
    assert_eq!(rollouts.calls(), ["agg2"]);

    // A fully completed job resumes with zero model calls and rollouts.
    Jobs::of(fx.dir()).resume(&job).unwrap();
    let again = FakeImprover::default();
    let none = FakeRollouts::default();
    let replay = fx.run(&job, &none, &again).unwrap();
    assert_eq!(replay.summary.status, "ok");
    assert!(again.labels().is_empty() && none.calls().is_empty());
    assert_eq!(
        (replay.summary.model_calls, replay.summary.rollouts_run),
        (0, 0)
    );
    assert_eq!(
        replay.proposals[0].proposal.id, done.proposals[0].proposal.id,
        "no duplicate proposal"
    );
}

/// Scenario: Edited cached harness.
#[test]
fn an_edited_cached_workspace_reruns_the_stage_and_its_dependents() {
    let fx = Fixture::new(&[]);
    let job = fx.job(1, EvolveMode::Parallel);
    let script_once = |r: &pmt_evolve::engine::ports::ImproverRequest<'_>, body: &str| {
        if r.label == "inv1" {
            let dir = r.workspace.join("context/harness/scripts");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("fix.py"), body).unwrap();
            std::fs::write(
                r.workspace.join("context/harness/TOOL_GUIDANCE.md"),
                "Run scripts/fix.py first.\n",
            )
            .unwrap();
        }
        Ok(edit_guardrails(r, &format!("- {}", r.label)))
    };
    let first = FakeImprover::with(move |r, _| script_once(r, "def fix():\n    return 1\n"));
    fx.run(&job, &FakeRollouts::default(), &first).unwrap();
    // Tamper with inv1's cached workspace.
    let sessions = fx.dir().join("evolve/cache/sessions");
    let tampered = std::fs::read_dir(&sessions)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.join("workspace/scripts/fix.py").is_file())
        .expect("inv1 cached");
    std::fs::write(
        tampered.join("workspace/scripts/fix.py"),
        "def fix():\n    return 2\n",
    )
    .unwrap();
    Jobs::of(fx.dir()).resume(&job).unwrap();
    let changed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&changed);
    let second = FakeImprover::with(move |r, _| {
        flag.store(true, Ordering::Relaxed);
        script_once(r, "def fix():\n    return 3\n")
    });
    let rollouts = FakeRollouts::default();
    fx.run(&job, &rollouts, &second).unwrap();
    let labels = second.labels();
    assert!(labels.contains(&"inv1".to_owned()), "{labels:?}");
    assert!(
        labels.contains(&"agg1".to_owned()),
        "a dependent re-ran: {labels:?}"
    );
    assert!(
        !labels.contains(&"dom1".to_owned()),
        "an independent stage stayed cached: {labels:?}"
    );
    assert!(rollouts.calls().contains(&"inv1".to_owned()));
    assert!(changed.load(Ordering::Relaxed));
}

/// Job lifecycle: expired jobs are pruned, except the summary of a job
/// whose proposal was adopted.
#[test]
fn expired_jobs_are_pruned_but_an_adopted_jobs_summary_stays() {
    let fx = Fixture::new(&[]);
    let kept = fx.job(1, EvolveMode::Parallel);
    let gone = fx.job_with(1, EvolveMode::Parallel, None, &[("other.txt", "x")]);
    let done = fx
        .run(&kept, &FakeRollouts::default(), &FakeImprover::default())
        .unwrap();
    fx.run(&gone, &FakeRollouts::default(), &FakeImprover::default())
        .unwrap();
    pmt_evolve::proposal::Proposals::of(fx.dir())
        .adopt(
            fx.dir(),
            &done.proposals[0].proposal.id,
            false,
            &Fixture::rules(),
        )
        .unwrap();
    assert!(
        pmt_evolve::engine::prune(fx.dir(), 3600)
            .unwrap()
            .is_empty(),
        "nothing expired yet"
    );
    for job in [&kept, &gone] {
        let path = fx.job_dir(job).join("job.toml");
        let text = std::fs::read_to_string(&path).unwrap();
        let old = text
            .lines()
            .map(|l| {
                if l.starts_with("created_at") {
                    "created_at = \"2020-01-01T00:00:00Z\""
                } else {
                    l
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, old).unwrap();
    }
    let mut pruned = pmt_evolve::engine::prune(fx.dir(), 3600).unwrap();
    pruned.sort();
    let mut both = vec![kept.clone(), gone.clone()];
    both.sort();
    assert_eq!(pruned, both);
    assert!(!fx.job_dir(&gone).exists());
    let mut left: Vec<String> = std::fs::read_dir(fx.job_dir(&kept))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["job.toml", "summary.json"]);
}
