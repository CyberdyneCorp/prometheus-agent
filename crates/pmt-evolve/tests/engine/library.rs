//! The experience library fed by the engine (spec: experience-library,
//! Library layout; Domain resolution for a job; Reflection workspace;
//! Experience distillation; Entries are written on adoption only;
//! Answer-free and secret-free entries; Index; Exposure to proposers;
//! Stream order within a domain).

use std::path::Path;
use std::sync::{Arc, Mutex};

use pmt_config::schema::EvolveMode;
use pmt_core::secret::Redactor;
use pmt_evolve::engine::ports::{ImproverFailure, ImproverReply, SessionKind};
use pmt_evolve::experience::library::EMPTY;
use pmt_evolve::experience::{self, Library, Outcome, Settings};
use pmt_evolve::job::Jobs;
use pmt_evolve::proposal::{Proposal, Proposals};

use crate::support::{
    FakeImprover, FakeRollouts, Fixture, edit_guardrails, read_json, reflecting, reflection_reply,
};

fn settings(fx: &Fixture) -> Settings {
    let config = fx.scope.config();
    Settings::of(&config.evolve, &config.experience, Redactor::default())
}

fn adopt(fx: &Fixture, proposal: &Proposal) -> Outcome {
    let rules = Fixture::rules();
    experience::adopt(fx.dir(), &proposal.id, false, &rules, &settings(fx))
        .unwrap()
        .1
}

fn entries(fx: &Fixture, domain: &str) -> Vec<String> {
    let dir = fx.dir().join("experience").join(domain).join("entries");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|d| {
            d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn listing(dir: &Path) -> Option<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    Some(names)
}

/// Scenario: Entry created.
#[test]
fn an_adopted_proposal_with_a_lesson_becomes_an_entry() {
    let fx = Fixture::new(&[]);
    let job = fx.job(1, EvolveMode::Parallel);
    let improver = reflecting(reflection_reply("high", Some("base1 step 4 dropped -2.50")));
    let done = fx.run(&job, &FakeRollouts::default(), &improver).unwrap();
    assert_eq!(done.summary.status, "ok");
    let proposal = &done.proposals[0].proposal;
    let lesson = proposal.lesson.as_ref().expect("a lesson");
    assert_eq!(lesson.cue, "ledgers with refunds");
    assert_eq!(proposal.domain, "accounting");
    assert!(entries(&fx, "accounting").is_empty(), "not before adoption");

    let reflection = fx.job_dir(&job).join("reflection/parallel1");
    for part in [
        "task.md",
        "harness",
        "base_harness",
        "rollouts/base1.md",
        "rollouts/agg1.md",
        "pool/inv1/harness.diff",
        "decisions/dom1.md",
    ] {
        assert!(reflection.join(part).exists(), "{part}");
    }
    let adoption = read_json(&reflection.join("adoption.json"));
    assert_eq!(adoption["chosen_label"], "agg1");
    assert!(
        improver
            .labels()
            .contains(&"experience.parallel1".to_owned())
    );
    let ledger = read_json(&fx.job_dir(&job).join("ledger.json"));
    assert_eq!(
        ledger["modes"][0]["reflection"]["label"],
        "experience.parallel1"
    );
    let events = std::fs::read_to_string(fx.job_dir(&job).join("events.jsonl")).unwrap();
    assert!(
        events.contains(r#""type":"experience.lesson.distilled""#)
            && events.contains(r#""confidence":"high""#),
        "{events}"
    );

    let Outcome::Written(written) = adopt(&fx, proposal) else {
        panic!("no entry written");
    };
    let names = entries(&fx, "accounting");
    assert_eq!(names.len(), 1);
    let re = regex::Regex::new(r"^\d{8}-sum-the-amounts-in-ledger-csv-[0-9a-f]{6}$").unwrap();
    assert!(re.is_match(&names[0]), "{names:?}");
    let dir = fx
        .dir()
        .join("experience/accounting/entries")
        .join(&names[0]);
    for part in ["EXPERIENCE.md", "entry.toml", "harness/GUARDRAILS.md"] {
        assert!(dir.join(part).is_file(), "{part}");
    }
    let doc = std::fs::read_to_string(dir.join("EXPERIENCE.md")).unwrap();
    for line in [
        "# Sum the amounts in ledger.csv",
        "- adopted: agg1 (job ",
        "- components: always-on edits only",
        "## Why this was chosen",
        "## What each edit did",
        "## Transferable lesson",
        "- confidence: high (because two rollouts agree)",
    ] {
        assert!(doc.contains(line), "{line}\n{doc}");
    }
    let meta: toml::Table =
        toml::from_str(&std::fs::read_to_string(dir.join("entry.toml")).unwrap()).unwrap();
    assert_eq!(meta["status"].as_str(), Some("active"));
    assert_eq!(meta["pinned"].as_bool(), Some(false));
    assert_eq!(meta["times_shown"].as_integer(), Some(0));
    assert_eq!(meta["k"].as_integer(), Some(1));
    assert_eq!(
        meta["harness_version"].as_str(),
        proposal_version(&fx, proposal).as_deref()
    );
    let index = std::fs::read_to_string(fx.dir().join("experience/accounting/INDEX.md")).unwrap();
    assert!(
        index.contains(&format!("## entries/{}/", written.meta.entry_id)),
        "{index}"
    );
    assert!(index.contains("- cue: ledgers with refunds"));
}

fn proposal_version(fx: &Fixture, proposal: &Proposal) -> Option<String> {
    Proposals::of(fx.dir())
        .get(&proposal.id)
        .unwrap()
        .adopted_version
}

/// Scenario: Rejected proposal.
#[test]
fn a_rejected_proposal_writes_no_entry() {
    let fx = Fixture::new(&[]);
    let job = fx.job(1, EvolveMode::Parallel);
    let improver = reflecting(reflection_reply("high", Some("base1 step 4")));
    let done = fx.run(&job, &FakeRollouts::default(), &improver).unwrap();
    let proposal = &done.proposals[0].proposal;
    assert!(proposal.lesson.is_some());
    let before = entries(&fx, "accounting");
    Proposals::of(fx.dir()).reject(&proposal.id).unwrap();
    assert_eq!(entries(&fx, "accounting"), before);
    assert!(before.is_empty());
}

/// Scenario: All edits confounded.
#[test]
fn a_no_entry_reflection_adopts_without_an_entry() {
    let fx = Fixture::new(&[]);
    let job = fx.job(1, EvolveMode::Parallel);
    let improver = reflecting(reflection_reply("low", None).replace("helped", "confounded"));
    let done = fx.run(&job, &FakeRollouts::default(), &improver).unwrap();
    let proposal = &done.proposals[0].proposal;
    assert_eq!(proposal.lesson, None);
    let text = std::fs::read_to_string(
        fx.dir()
            .join("evolve/proposals")
            .join(format!("{}.toml", proposal.id)),
    )
    .unwrap();
    assert!(!text.contains("[lesson]"), "{text}");
    let outcome = adopt(&fx, proposal);
    assert!(matches!(outcome, Outcome::NoLesson));
    assert_eq!(
        outcome.message(),
        "no transferable lesson; nothing will be added to the experience library"
    );
    assert!(entries(&fx, "accounting").is_empty());
    assert!(
        proposal_version(&fx, proposal).is_some(),
        "the version is still written"
    );
}

/// Scenario: Failed selection skips reflection.
#[test]
fn a_failed_selection_runs_no_reflection() {
    let fx = Fixture::new(&[]);
    let job = fx.job(2, EvolveMode::Parallel);
    let improver = FakeImprover::with(|r, _| {
        if r.kind == SessionKind::Select {
            return Ok(ImproverReply {
                text: "```choice\nZZ\n```".into(),
                usd: 0.0,
            });
        }
        Ok(edit_guardrails(r, "- rule"))
    });
    let done = fx.run(&job, &FakeRollouts::default(), &improver).unwrap();
    assert_eq!(done.summary.modes[0].status, "selection failed");
    assert!(!fx.job_dir(&job).join("reflection").exists());
    assert!(
        !improver
            .labels()
            .iter()
            .any(|l| l.starts_with("experience."))
    );
    assert!(entries(&fx, "accounting").is_empty());
}

/// Scenario: Verdict in lesson.
#[test]
fn a_verdict_in_the_lesson_rejects_the_entry_but_not_the_adoption() {
    let fx = Fixture::new(&[]);
    let job = fx.job(1, EvolveMode::Parallel);
    let improver = reflecting(reflection_reply(
        "high",
        Some(r#"the grader said {"pass_rate": 1}"#),
    ));
    let done = fx.run(&job, &FakeRollouts::default(), &improver).unwrap();
    let proposal = &done.proposals[0].proposal;
    let outcome = adopt(&fx, proposal);
    assert!(
        matches!(&outcome, Outcome::Rejected(why) if why == "verdict tripwire"),
        "{outcome:?}"
    );
    let saved = Proposals::of(fx.dir()).get(&proposal.id).unwrap();
    assert_eq!(saved.lesson_rejected.as_deref(), Some("verdict tripwire"));
    assert!(saved.adopted_version.is_some());
    assert!(entries(&fx, "accounting").is_empty());
}

/// Per improver label, the entries its evidence `library/` listed
/// (`None`: no `library/`).
type Seen = Mutex<Vec<(String, Option<Vec<String>>)>>;

/// An improver recording what each session's evidence library held.
fn watching(seen: Arc<Seen>) -> FakeImprover {
    FakeImprover::with(move |r, _| {
        let library = r.workspace.join("context/library");
        seen.lock().unwrap().push((
            r.label.to_owned(),
            listing(&library).map(|_| listing(&library.join("entries")).unwrap_or_default()),
        ));
        if r.kind == SessionKind::Reflect {
            return Ok(ImproverReply {
                text: reflection_reply("high", Some("base1 step 4")),
                usd: 0.0,
            });
        }
        Ok(edit_guardrails(r, &format!("- rule from {}", r.label)))
    })
}

fn seen_by(seen: &Seen, label: &str) -> Option<Vec<String>> {
    seen.lock()
        .unwrap()
        .iter()
        .find(|(l, _)| l == label)
        .and_then(|(_, s)| s.clone())
}

/// Scenarios: Empty domain; Index without entries; Second job sees the
/// first.
#[test]
fn the_second_job_of_a_domain_sees_exactly_the_first_jobs_entry() {
    let fx = Fixture::new(&[]);
    let first = fx.job(1, EvolveMode::Parallel);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let done = fx
        .run(
            &first,
            &FakeRollouts::default(),
            &watching(Arc::clone(&seen)),
        )
        .unwrap();
    let index = std::fs::read_to_string(fx.dir().join("experience/accounting/INDEX.md")).unwrap();
    assert!(
        index.contains("# Experience library — accounting") && index.contains(EMPTY),
        "{index}"
    );
    assert_eq!(
        seen_by(&seen, "inv1"),
        None,
        "an index alone gives no library/"
    );
    assert!(!fx.job_dir(&first).join("library").exists());
    let Outcome::Written(written) = adopt(&fx, &done.proposals[0].proposal) else {
        panic!("no entry");
    };

    let second = fx.job(1, EvolveMode::Parallel);
    seen.lock().unwrap().clear();
    fx.run(
        &second,
        &FakeRollouts::default(),
        &watching(Arc::clone(&seen)),
    )
    .unwrap();
    assert_eq!(
        seen_by(&seen, "inv1"),
        Some(vec![written.meta.entry_id.clone()])
    );
}

/// Exposure to proposers: entries adopted after a job started stay out of
/// its evidence, including on resume.
#[test]
fn a_job_started_before_an_adoption_never_sees_it_even_on_resume() {
    let fx = Fixture::new(&[]);
    let first = fx.job(1, EvolveMode::Parallel);
    let improver = reflecting(reflection_reply("high", Some("base1 step 4")));
    let done = fx.run(&first, &FakeRollouts::default(), &improver).unwrap();
    let pending = done.proposals[0].proposal.clone();

    // The second job (another task, so nothing is cached) starts, then
    // stops at its composition.
    let second = fx.job_with(
        1,
        EvolveMode::Parallel,
        None,
        &[("notes.txt", "another task")],
    );
    let cancel = fx.job_dir(&second).join(pmt_evolve::engine::CANCEL_FILE);
    let stopping = FakeImprover::with(move |r, _| {
        if r.label == "agg1" {
            std::fs::write(&cancel, "").unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
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
        .run(&second, &FakeRollouts::default(), &stopping)
        .unwrap();
    assert_eq!(stopped.summary.status, "cancelled");

    // The first job's proposal is adopted while the second is stopped.
    assert!(matches!(adopt(&fx, &pending), Outcome::Written(_)));
    Jobs::of(fx.dir()).resume(&second).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let resumed = fx
        .run(
            &second,
            &FakeRollouts::default(),
            &watching(Arc::clone(&seen)),
        )
        .unwrap();
    assert_eq!(resumed.summary.status, "ok");
    assert_eq!(
        seen_by(&seen, "agg1"),
        None,
        "the snapshot predates the adoption"
    );
    assert!(!fx.job_dir(&second).join("library").exists());
}

/// Scenario: Classifier limited to known domains.
#[test]
fn a_job_without_a_domain_is_filed_under_a_known_domain_or_general() {
    let fx = Fixture::new(&[]);
    let library = Library::of(fx.dir());
    library.ensure("accounting").unwrap();
    library.ensure("devops").unwrap();
    let job = fx.job_in(1, EvolveMode::Parallel, None, &[], None);
    assert_eq!(Jobs::of(fx.dir()).get(&job).unwrap().domain, "");
    let improver = FakeImprover::with(|r, _| {
        if r.kind == SessionKind::Classify {
            assert!(
                r.mission.contains("accounting, devops, general"),
                "{}",
                r.mission
            );
            return Ok(ImproverReply {
                text: "marketing".into(),
                usd: 0.0,
            });
        }
        Ok(edit_guardrails(r, "- rule"))
    });
    let done = fx.run(&job, &FakeRollouts::default(), &improver).unwrap();
    assert_eq!(done.summary.status, "ok");
    assert_eq!(
        improver
            .labels()
            .iter()
            .filter(|l| *l == "classify")
            .count(),
        1
    );
    assert_eq!(Jobs::of(fx.dir()).get(&job).unwrap().domain, "general");
    let meta: toml::Table =
        toml::from_str(&std::fs::read_to_string(fx.job_dir(&job).join("task/task.toml")).unwrap())
            .unwrap();
    assert_eq!(meta["domain"].as_str(), Some("general"));
    assert_eq!(meta["domain_source"].as_str(), Some("classifier"));
    let ledger = read_json(&fx.job_dir(&job).join("ledger.json"));
    assert!(
        ledger["costs"]["classification"].as_f64().unwrap() > 0.0,
        "{ledger}"
    );
    assert!(
        ledger["modes"][0]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["label"] != "classify"),
        "outside the R/P identity"
    );
}
