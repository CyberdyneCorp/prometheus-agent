use pmt_config::schema::EvolveMode;
use pmt_evolve::engine::ports::{ImproverReply, SessionKind};

use crate::support::{FakeImprover, FakeRollouts, Fixture, edit_guardrails};

/// Scenario: Unknown id.
#[test]
fn an_unknown_choice_fails_the_selection_and_makes_no_proposal() {
    let fx = Fixture::new(&[]);
    let job = fx.job(2, EvolveMode::Parallel);
    let improver = FakeImprover::with(|r, _| {
        if r.kind == SessionKind::Select {
            let ids: Vec<_> = std::fs::read_dir(r.workspace.join("context/candidates"))
                .unwrap()
                .collect();
            assert_eq!(ids.len(), 2, "A and B");
            assert!(
                !r.workspace.join("context/harness").exists(),
                "the selector sees no harness"
            );
            return Ok(ImproverReply {
                text: "```choice\nZ\n```\n<why>z</why>".into(),
                usd: 0.0,
            });
        }
        Ok(edit_guardrails(r, &format!("- {}", r.label)))
    });
    let done = fx.run(&job, &FakeRollouts::default(), &improver).unwrap();
    assert_eq!(done.summary.modes[0].status, "selection failed");
    assert_eq!(done.summary.status, "partial");
    assert!(done.proposals.is_empty());
    assert!(
        pmt_evolve::proposal::Proposals::of(fx.dir())
            .list()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn the_selector_sees_only_anonymous_candidates_and_its_choice_maps_back() {
    let fx = Fixture::new(&[]);
    let job = fx.job(2, EvolveMode::Parallel);
    let improver = FakeImprover::with(|r, _| {
        if r.kind == SessionKind::Select {
            let a = std::fs::read_to_string(r.workspace.join("context/candidates/A.md")).unwrap();
            assert!(
                a.starts_with("# Candidate A\n\n") && a.contains(" characters, "),
                "{a}"
            );
            assert!(!a.contains("agg"), "no label leaks: {a}");
            return Ok(ImproverReply {
                text: "```choice\nB\n```\n<why>B is complete</why>".into(),
                usd: 0.0,
            });
        }
        Ok(edit_guardrails(r, &format!("- {}", r.label)))
    });
    let done = fx.run(&job, &FakeRollouts::default(), &improver).unwrap();
    let mode = &done.summary.modes[0];
    let selected = mode.selected.as_ref().unwrap();
    assert_eq!(selected.id, "B");
    assert_eq!(selected.reason, "B is complete");
    let mapped = mode.candidates.iter().find(|(_, id)| id == "B").unwrap();
    assert_eq!(selected.label, mapped.0);
    assert_eq!(done.proposals[0].proposal.label, mapped.0);
}

#[test]
fn at_k1_no_selector_runs() {
    let fx = Fixture::new(&[]);
    let job = fx.job(1, EvolveMode::Parallel);
    let improver = FakeImprover::default();
    let done = fx.run(&job, &FakeRollouts::default(), &improver).unwrap();
    assert!(!improver.labels().iter().any(|l| l.starts_with("select")));
    assert_eq!(
        done.summary.modes[0].selected.as_ref().unwrap().selected_by,
        "single"
    );
    assert_eq!(
        done.summary.modes[0].selected.as_ref().unwrap().label,
        "agg1"
    );
}

/// Scenario: User picks a candidate.
#[test]
fn a_user_selection_pauses_then_continues_with_the_chosen_id() {
    let fx = Fixture::new(&[r#"evolve.selection="user""#]);
    let job = fx.job(2, EvolveMode::Parallel);
    let rollouts = FakeRollouts::default();
    let improver = FakeImprover::default();
    let paused = fx.run(&job, &rollouts, &improver).unwrap();
    assert_eq!(paused.summary.status, "awaiting-selection");
    assert!(paused.proposals.is_empty());
    assert!(
        !improver.labels().iter().any(|l| l.starts_with("select")),
        "no selector session"
    );
    let candidates = fx.job_dir(&job).join("modes/parallel/candidates");
    assert!(candidates.join("A.md").is_file() && candidates.join("B.md").is_file());
    pmt_evolve::job::Jobs::of(fx.dir())
        .choose(&job, "B")
        .unwrap();
    let before = improver.labels().len();
    let done = fx.run(&job, &rollouts, &improver).unwrap();
    assert_eq!(done.summary.status, "ok");
    assert_eq!(
        improver.labels()[before..],
        ["experience.parallel2"],
        "everything else was cached; the reflection follows the user's choice"
    );
    let selected = done.summary.modes[0].selected.clone().unwrap();
    assert_eq!(
        (selected.id.as_str(), selected.selected_by.as_str()),
        ("B", "user")
    );
    let mapped = &done.summary.modes[0]
        .candidates
        .iter()
        .find(|(_, id)| id == "B")
        .unwrap()
        .0;
    assert_eq!(&done.proposals[0].proposal.label, mapped);
}
