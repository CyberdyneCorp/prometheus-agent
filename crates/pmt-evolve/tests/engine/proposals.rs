use pmt_config::harness::{HarnessDir, Source, Tree};
use pmt_config::schema::EvolveMode;
use pmt_evolve::engine::ports::ImproverReply;
use pmt_evolve::proposal::{Proposals, Status};

use crate::support::{FakeImprover, FakeRollouts, Fixture, edit_guardrails};

fn adding_a_script() -> FakeImprover {
    FakeImprover::with(|r, _| {
        if r.label.starts_with("agg") {
            let dir = r.workspace.join("context/harness/scripts");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("normalize.py"),
                "def normalize(x):\n    return x\n",
            )
            .unwrap();
            return Ok(ImproverReply {
                text: "added scripts/normalize.py (rollouts/base1.md step 3)".into(),
                usd: 0.0,
            });
        }
        Ok(edit_guardrails(r, "- ok"))
    })
}

/// Scenario: Auto policy with a new script.
#[test]
fn auto_policy_waits_for_approval_when_a_script_is_added() {
    let fx = Fixture::new(&[r#"evolve.adoption.policy="auto""#]);
    let job = fx.job(1, EvolveMode::Parallel);
    let before = HarnessDir::of(fx.dir()).active_id().unwrap();
    let done = fx
        .run(&job, &FakeRollouts::default(), &adding_a_script())
        .unwrap();
    let proposal = &done.proposals[0].proposal;
    assert_eq!(proposal.status, Status::Pending);
    assert!(proposal.approval_required);
    assert!(done.adopted().is_none());
    assert!(
        done.notices
            .iter()
            .any(|n| n.contains("approval is required")),
        "{:?}",
        done.notices
    );
    assert_eq!(HarnessDir::of(fx.dir()).active_id().unwrap(), before);
}

#[test]
fn auto_policy_adopts_prose_changes_as_an_evolve_version() {
    let fx = Fixture::new(&[r#"evolve.adoption.policy="auto""#]);
    let job = fx.job(1, EvolveMode::Parallel);
    let done = fx
        .run(&job, &FakeRollouts::default(), &FakeImprover::default())
        .unwrap();
    let adopted = done.adopted().expect("adopted");
    let harness = HarnessDir::of(fx.dir());
    assert_eq!(harness.active_id().unwrap(), adopted.version.id);
    let info = harness.version(&adopted.version.id).unwrap();
    assert_eq!(info.source, Source::Evolve);
    assert_eq!(info.job_id.as_deref(), Some(job.as_str()));
    assert_eq!(done.proposals[0].proposal.status, Status::Adopted);
    let events = pmt_evolve::engine::events::JobLog::read(&fx.job_dir(&job));
    let kinds: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    for kind in [
        "evolve.job.started",
        "evolve.stage.finished",
        "evolve.rollout.finished",
        "evolve.proposal.ready",
        "evolve.proposal.adopted",
        "evolve.job.finished",
    ] {
        assert!(kinds.contains(&kind), "{kind} in {kinds:?}");
    }
}

/// Scenario: Manual edit after job (stale proposal protection), and a
/// clean rebase.
#[test]
fn a_stale_proposal_is_refused_unless_it_rebases_cleanly() {
    let fx = Fixture::new(&[]);
    let job = fx.job(1, EvolveMode::Parallel);
    let done = fx
        .run(&job, &FakeRollouts::default(), &FakeImprover::default())
        .unwrap();
    let id = done.proposals[0].proposal.id.clone();
    let base = done.proposals[0].proposal.base_version.clone();
    let harness = HarnessDir::of(fx.dir());
    let mut manual = harness.tree(&base).unwrap();
    manual.files.insert("SYSTEM.md".into(), "Be brief.".into());
    let edited = harness.write(&manual, Source::Manual, None).unwrap();
    harness.activate(&edited.id).unwrap();
    let store = Proposals::of(fx.dir());
    let err = store
        .adopt(fx.dir(), &id, false, &Fixture::rules())
        .unwrap_err();
    assert_eq!(err.exit_code(), 65);
    assert_eq!(
        err.message(),
        format!(
            "active harness changed since this proposal was produced (base {base}, active {}); re-run the job or adopt with --rebase",
            edited.id
        )
    );
    assert_eq!(harness.active_id().unwrap(), edited.id);
    let adopted = store.adopt(fx.dir(), &id, true, &Fixture::rules()).unwrap();
    let tree: Tree = harness.tree(&adopted.version.id).unwrap();
    assert_eq!(
        tree.get("SYSTEM.md"),
        Some("Be brief."),
        "the manual edit stays"
    );
    assert!(
        tree.get("GUARDRAILS.md")
            .unwrap()
            .contains("rule from agg1")
    );
}

/// Mode `both`: adopting one proposal supersedes the other.
#[test]
fn adopting_one_of_two_proposals_supersedes_the_other() {
    let fx = Fixture::new(&[]);
    let job = fx.job(1, EvolveMode::Both);
    let done = fx
        .run(&job, &FakeRollouts::default(), &FakeImprover::default())
        .unwrap();
    let ids: Vec<String> = done
        .proposals
        .iter()
        .map(|p| p.proposal.id.clone())
        .collect();
    let store = Proposals::of(fx.dir());
    let adopted = store
        .adopt(fx.dir(), &ids[1], false, &Fixture::rules())
        .unwrap();
    assert_eq!(adopted.superseded, [ids[0].clone()]);
    assert_eq!(store.get(&ids[0]).unwrap().status, Status::Superseded);
    assert_eq!(
        store.reject(&ids[0]).unwrap_err().exit_code(),
        65,
        "only pending proposals change"
    );
}

#[test]
fn an_unchanged_final_yields_no_change() {
    let fx = Fixture::new(&[]);
    let job = fx.job(1, EvolveMode::Parallel);
    let idle = FakeImprover::with(|r, _| {
        if r.kind == pmt_evolve::engine::ports::SessionKind::Select {
            return Ok(edit_guardrails(r, ""));
        }
        Ok(ImproverReply {
            text: "the harness is fine as it is".into(),
            usd: 0.0,
        })
    });
    let done = fx.run(&job, &FakeRollouts::default(), &idle).unwrap();
    assert_eq!(done.proposals[0].proposal.status, Status::NoChange);
}

/// Scenario: Both modes under the auto policy. One proposal per mode, at
/// most one adopted; the other is superseded and the job still ends `ok`.
#[test]
fn auto_policy_with_both_modes_adopts_one_and_supersedes_the_other() {
    let fx = Fixture::new(&[r#"evolve.adoption.policy="auto""#]);
    let job = fx.job(1, EvolveMode::Both);
    let done = fx
        .run(&job, &FakeRollouts::default(), &FakeImprover::default())
        .unwrap();
    assert_eq!(done.summary.status, "ok");
    let statuses: Vec<Status> = done.proposals.iter().map(|o| o.proposal.status).collect();
    assert_eq!(statuses, [Status::Adopted, Status::Superseded]);
    let adopted = done.adopted().expect("one adopted");
    assert_eq!(
        HarnessDir::of(fx.dir()).active_id().unwrap(),
        adopted.version.id
    );
    assert!(fx.job_dir(&job).join("ledger.json").is_file());
    let events = pmt_evolve::engine::events::JobLog::read(&fx.job_dir(&job));
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "evolve.job.finished" && e["payload"]["status"] == "ok"),
        "{events:?}"
    );
    assert!(
        done.notices.iter().any(|n| n.contains("superseded")),
        "{:?}",
        done.notices
    );
}

/// Scenario: Both modes under the auto policy (a failed automatic
/// adoption). A manual edit during the job moves `ACTIVE`, so the
/// automatic adoption is refused: the proposal stays pending and the job
/// still ends `ok`.
#[test]
fn a_refused_automatic_adoption_leaves_the_proposal_pending_and_the_job_ok() {
    let fx = Fixture::new(&[r#"evolve.adoption.policy="auto""#]);
    let job = fx.job(1, EvolveMode::Parallel);
    let profile = fx.dir().to_path_buf();
    let improver = FakeImprover::with(move |r, _| {
        if r.label == "agg1" {
            let harness = HarnessDir::of(&profile);
            let mut tree = harness.tree(&harness.active_id().unwrap()).unwrap();
            tree.files.insert("SYSTEM.md".into(), "manual edit".into());
            let version = harness.write(&tree, Source::Manual, None).unwrap();
            harness.activate(&version.id).unwrap();
        }
        Ok(edit_guardrails(r, "- ok"))
    });
    let done = fx.run(&job, &FakeRollouts::default(), &improver).unwrap();
    assert_eq!(done.summary.status, "ok");
    assert!(done.adopted().is_none());
    assert_eq!(done.proposals[0].proposal.status, Status::Pending);
    assert!(
        done.notices
            .iter()
            .any(|n| n.contains("could not be adopted automatically")),
        "{:?}",
        done.notices
    );
    assert!(fx.job_dir(&job).join("ledger.json").is_file());
}
