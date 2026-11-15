//! The kanban upgrades on a board (spec: kanban, PR completion contracts;
//! Respawn guard and dispatch dry run; Swarm topology helper; Orchestrator
//! guidance, specify and decompose; Subscription delivery modes and
//! inheritance; Worker lanes; Goal-mode cards; plugin-system, Kanban
//! lifecycle observer hooks).

mod support;

use std::sync::{Arc, Mutex};

use pmt_kanban::contract::{Acceptance, CheckRun, Evidence, PrChecks, PrRef, judge};
use pmt_kanban::dispatch::{SpawnRequest, Spawner};
use pmt_kanban::model::Event;
use pmt_kanban::notify::{DeliveryMode, Leg, NewSubscription};
use pmt_kanban::observe::Observer;
use pmt_kanban::orchestrate::{Graph, SwarmSpec};
use pmt_kanban::{
    Actor, Board, ClaimOutcome, Completion, Filter, Handoff, NewTask, Profiles, Role, Status,
};
use support::{Home, Sleepers, T0, dispatcher, kinds, profiles, settings, user};

/// A GitHub stand-in: PR 41 has a red `ci/test`, PR 42 is green.
struct FakeGitHub;

impl PrChecks for FakeGitHub {
    fn evaluate(&self, _profile: &str, pr: &PrRef) -> Acceptance {
        let conclusion = if pr.number == 41 {
            "failure"
        } else {
            "success"
        };
        judge(
            pr,
            &Evidence {
                head_sha: format!("sha{}", pr.number),
                required: vec!["ci/test".to_owned()],
                runs: vec![CheckRun {
                    id: 100 + i64::try_from(pr.number).unwrap(),
                    name: "ci/test".to_owned(),
                    status: "completed".to_owned(),
                    conclusion: Some(conclusion.to_owned()),
                }],
                statuses: Vec::new(),
            },
        )
    }
}

fn contract_task(board: &mut Board) -> (String, Actor) {
    let mut new = NewTask::titled("ship the api change");
    new.assignee = Some("coder".to_owned());
    new.completion_contract = Some("acme/api".to_owned());
    let id = board.create(&user(), &new, &profiles, T0).unwrap().id;
    let ClaimOutcome::Claimed(claim) = board
        .claim(&Actor::dispatcher(), &id, "coder", Role::Worker, 60_000, T0)
        .unwrap()
    else {
        panic!("not claimed");
    };
    (id, Actor::worker("coder", &claim.run_id))
}

fn finish_with(pr: &str) -> Completion {
    Completion {
        result: Some("PR opened".to_owned()),
        metadata: Some(serde_json::json!({ "published_pr": pr })),
        ..Completion::default()
    }
}

/// Scenario: Red required check blocks completion.
#[test]
fn a_red_required_check_blocks_completion() {
    let home = Home::new();
    let mut board = home.board().with_pr_checks(Arc::new(FakeGitHub));
    let (id, worker) = contract_task(&mut board);
    let err = board
        .complete(
            &worker,
            &id,
            &finish_with("https://github.com/acme/api/pull/41"),
            T0,
        )
        .unwrap_err();
    assert!(
        err.message().contains("ci/test (failure)"),
        "{}",
        err.message()
    );
    assert_eq!(board.task(&id).unwrap().status, Status::Running);
    let event = board
        .events(Some(&id))
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "pr_acceptance")
        .unwrap();
    assert_eq!(event.payload["classification"], "checks");
    assert_eq!(event.payload["failing"]["ci/test"], "failure");
    assert_eq!(event.payload["head_sha"], "sha41");
    assert_eq!(event.payload["check_ids"][0], 141);
    assert!(event.payload["next_step"].as_str().unwrap().contains("fix"));
}

/// Scenario: Retry cannot substitute a sibling PR.
#[test]
fn a_retry_cannot_substitute_a_sibling_pr() {
    let home = Home::new();
    let mut board = home.board().with_pr_checks(Arc::new(FakeGitHub));
    let (id, worker) = contract_task(&mut board);
    let red = finish_with("https://github.com/acme/api/pull/41");
    assert!(board.complete(&worker, &id, &red, T0).is_err());
    let sibling = finish_with("https://github.com/acme/api/pull/42");
    let err = board.complete(&worker, &id, &sibling, T0).unwrap_err();
    assert!(
        err.message()
            .contains("completion contract already bound to https://github.com/acme/api/pull/41"),
        "{}",
        err.message()
    );
    assert_eq!(board.task(&id).unwrap().status, Status::Running);
    // A PR of another repository is refused too, and a local-only task
    // needs no evidence.
    let foreign = finish_with("https://github.com/acme/web/pull/42");
    assert!(board.complete(&worker, &id, &foreign, T0).is_err());
}

/// A repository contract without a checker (a surface that cannot read
/// GitHub) or without `published_pr` refuses; an exact contract accepts
/// its own green PR.
#[test]
fn contracts_refuse_without_evidence_and_accept_a_green_pr() {
    let home = Home::new();
    let mut plain = home.board();
    let (id, worker) = contract_task(&mut plain);
    let no_pr = Completion {
        result: Some("done".to_owned()),
        ..Completion::default()
    };
    let err = plain.complete(&worker, &id, &no_pr, T0).unwrap_err();
    assert!(err.message().contains("requires metadata.published_pr"));
    let green = finish_with("https://github.com/acme/api/pull/42");
    let err = plain.complete(&worker, &id, &green, T0).unwrap_err();
    assert!(err.message().contains("cannot read GitHub checks"));
    let mut checked = home.board().with_pr_checks(Arc::new(FakeGitHub));
    assert_eq!(
        checked.complete(&worker, &id, &green, T0).unwrap(),
        Status::Done
    );
    assert_eq!(
        checked.task(&id).unwrap().pr_url.as_deref(),
        Some("https://github.com/acme/api/pull/42")
    );
}

/// Scenario: Fresh PR is not re-worked.
#[test]
fn a_fresh_pr_comment_guards_the_task_and_the_dry_run_says_so() {
    let home = Home::new();
    let mut board = home.board();
    let mut new = NewTask::titled("follow up the PR");
    new.assignee = Some("coder".to_owned());
    let id = board.create(&user(), &new, &profiles, T0).unwrap().id;
    let two_hours = 2 * 3_600_000;
    board
        .comment(
            &Actor::worker("coder", "r_old"),
            &id,
            "opened https://github.com/acme/api/pull/41",
            T0,
        )
        .unwrap();
    let spawner = Sleepers::default();
    let d = dispatcher(&home, &spawner, settings());
    let plans = d.plan(T0 + two_hours).unwrap();
    assert_eq!(plans[0].guarded, vec![(id.clone(), "active_pr".to_owned())]);
    assert!(
        kinds(&board, &id).iter().all(|k| k != "respawn_guarded"),
        "a dry run changes nothing"
    );
    let summary = d.tick(T0 + two_hours).unwrap();
    assert!(spawner.spawned.borrow().is_empty());
    assert_eq!(
        summary[0].guarded,
        vec![(id.clone(), "active_pr".to_owned())]
    );
    d.tick(T0 + two_hours + 1).unwrap();
    let guarded = kinds(&board, &id)
        .iter()
        .filter(|k| *k == "respawn_guarded")
        .count();
    assert_eq!(guarded, 1, "one event per guard, never a failure");
    assert_eq!(board.task(&id).unwrap().consecutive_failures, 0);
    // Reassigned to another profile, the guard lifts; a day later it is
    // gone anyway.
    board
        .assign(&user(), &id, Some("ops"), &profiles, T0 + two_hours + 2)
        .unwrap();
    assert!(d.plan(T0 + two_hours + 3).unwrap()[0].guarded.is_empty());
}

/// A run that finished recently holds its task when review reopens it;
/// a deliberate re-queue lifts the guard.
#[test]
fn a_recent_success_holds_until_a_deliberate_requeue() {
    let home = Home::new();
    let mut board = home.board();
    let mut new = NewTask::titled("done once");
    new.assignee = Some("coder".to_owned());
    new.review = Some(true);
    let id = board.create(&user(), &new, &profiles, T0).unwrap().id;
    let ClaimOutcome::Claimed(claim) = board
        .claim(&Actor::dispatcher(), &id, "coder", Role::Worker, 60_000, T0)
        .unwrap()
    else {
        panic!("not claimed")
    };
    let worker = Actor::worker("coder", &claim.run_id);
    let finished = Completion {
        result: Some("ok".to_owned()),
        ..Completion::default()
    };
    assert_eq!(
        board.complete(&worker, &id, &finished, T0).unwrap(),
        Status::Review
    );
    board
        .reopen_review(&user(), &id, Some("not yet"), T0 + 1)
        .unwrap();
    let spawner = Sleepers::default();
    let d = dispatcher(&home, &spawner, settings());
    assert_eq!(
        d.plan(T0 + 2).unwrap()[0].guarded,
        vec![(id.clone(), "recent_success".to_owned())]
    );
    assert!(
        d.plan(T0 + 3_600_001).unwrap()[0].guarded.is_empty(),
        "the window passed"
    );
    board
        .block(
            &user(),
            &id,
            pmt_kanban::BlockKind::NeedsInput,
            "hold",
            T0 + 3,
        )
        .unwrap();
    board.unblock(&user(), &id, T0 + 4).unwrap();
    assert!(
        d.plan(T0 + 5).unwrap()[0].guarded.is_empty(),
        "unblock re-queues"
    );
}

/// Scenario: Atomic swarm creation.
#[test]
fn a_swarm_is_created_whole_with_the_verifier_gated_on_every_worker() {
    let home = Home::new();
    let mut board = home.board();
    let workers = ["writer".to_owned(), "coder".to_owned(), "ops".to_owned()];
    let swarm = board
        .swarm(
            &user(),
            &SwarmSpec {
                goal: "failover plan",
                workers: &workers,
                verifier: "qa",
                synthesizer: "writer",
            },
            &profiles,
            T0,
        )
        .unwrap();
    assert_eq!(board.tasks(&Filter::default()).unwrap().len(), 6);
    assert_eq!(board.task(&swarm.root).unwrap().status, Status::Done);
    let blackboard = &board.comments(&swarm.root).unwrap()[0].body;
    assert!(blackboard.contains("\"entries\":[]"), "{blackboard}");
    for worker in &swarm.workers {
        assert_eq!(board.task(worker).unwrap().status, Status::Ready);
    }
    assert_eq!(board.task(&swarm.verifier).unwrap().status, Status::Todo);
    assert_eq!(board.parents(&swarm.verifier).unwrap().len(), 3);
    assert_eq!(board.task(&swarm.synthesizer).unwrap().status, Status::Todo);
    let ghost = ["writer".to_owned(), "ghost".to_owned()];
    let err = board
        .swarm(
            &user(),
            &SwarmSpec {
                goal: "failover plan",
                workers: &ghost,
                verifier: "qa",
                synthesizer: "writer",
            },
            &profiles,
            T0,
        )
        .unwrap_err();
    assert_eq!(err.message(), "unknown profile 'ghost'");
    assert_eq!(err.exit_code(), 66);
    assert_eq!(board.tasks(&Filter::default()).unwrap().len(), 6);
}

/// Scenario: Decompose rejects unknown assignees.
#[test]
fn a_decomposition_with_an_unknown_assignee_creates_nothing() {
    let home = Home::new();
    let mut board = home.board();
    let id = board
        .create(&user(), &NewTask::titled("big goal"), &profiles, T0)
        .unwrap()
        .id;
    let graph = Graph::parse(
        r#"Here: {"tasks": [{"key": "a", "title": "research", "assignee": "writer"},
            {"key": "b", "title": "build", "assignee": "nobody", "depends_on": ["a"]}]}"#,
    )
    .unwrap();
    let err = board
        .decompose(&user(), &id, &graph, true, &profiles, T0)
        .unwrap_err();
    assert!(err.message().contains("nobody"), "{}", err.message());
    assert_eq!(err.exit_code(), 65);
    assert_eq!(board.tasks(&Filter::default()).unwrap().len(), 1);
    let cycle = Graph::parse(
        r#"{"tasks": [{"key": "a", "title": "x", "depends_on": ["b"]}, {"key": "b", "title": "y", "depends_on": ["a"]}]}"#,
    )
    .unwrap();
    assert!(
        board
            .decompose(&user(), &id, &cycle, true, &profiles, T0)
            .is_err()
    );
    let good = Graph::parse(
        r#"{"tasks": [{"key": "a", "title": "research", "assignee": "writer"},
            {"key": "b", "title": "build", "assignee": "coder", "depends_on": ["a"]}]}"#,
    )
    .unwrap();
    let ids = board
        .decompose(&user(), &id, &good, true, &profiles, T0)
        .unwrap();
    assert_eq!(board.task(&ids["a"]).unwrap().status, Status::Ready);
    assert_eq!(board.task(&ids["b"]).unwrap().status, Status::Todo);
    assert_eq!(
        board.task(&id).unwrap().status,
        Status::Todo,
        "the goal waits for its children"
    );
    let manual = Graph::parse(r#"{"tasks": [{"key": "c", "title": "solo"}]}"#).unwrap();
    let other = board
        .create(&user(), &NewTask::titled("manual"), &profiles, T0)
        .unwrap()
        .id;
    let ids = board
        .decompose(&user(), &other, &manual, false, &profiles, T0)
        .unwrap();
    assert_eq!(board.task(&ids["c"]).unwrap().status, Status::Todo);
}

/// Scenario: Specify promotes a triage idea (the board half: the body is
/// replaced and the task moves to `todo`).
#[test]
fn specify_moves_a_triage_idea_to_todo() {
    let home = Home::new();
    let mut board = home.board();
    let mut new = NewTask::titled("add rate limiting");
    new.initial_status = Some(Status::Triage);
    let id = board.create(&user(), &new, &profiles, T0).unwrap().id;
    board
        .specify(
            &user(),
            &id,
            "Limit to 10 rps.\n\nAcceptance criteria:\n- 429 above",
            T0,
        )
        .unwrap();
    let task = board.task(&id).unwrap();
    assert_eq!(task.status, Status::Todo);
    assert!(task.body.contains("Acceptance criteria"));
    assert!(
        board.specify(&user(), &id, "again", T0).is_err(),
        "only triage tasks"
    );
}

/// A worker-created task copies its owning task's subscriptions, modes
/// unchanged; a `wake` subscription only wakes, on wake events.
#[test]
fn worker_created_tasks_inherit_subscriptions_and_modes_route_legs() {
    let home = Home::new();
    let mut board = home.board();
    let mut new = NewTask::titled("parent");
    new.assignee = Some("coder".to_owned());
    let id = board.create(&user(), &new, &profiles, T0).unwrap().id;
    let mut sub = NewSubscription::notify("discord", "c1", Some("t9"), "writer");
    sub.mode = DeliveryMode::NotifyWake;
    sub.chat_type = Some("thread".to_owned());
    board.subscribe(&user(), &id, &sub, T0).unwrap();
    let ClaimOutcome::Claimed(claim) = board
        .claim(&Actor::dispatcher(), &id, "coder", Role::Worker, 60_000, T0)
        .unwrap()
    else {
        panic!("not claimed")
    };
    let worker = Actor::worker("coder", &claim.run_id);
    let child = board
        .create(&worker, &NewTask::titled("follow-up"), &profiles, T0)
        .unwrap()
        .id;
    let inherited = board.subscriptions(Some(&child)).unwrap();
    assert_eq!(inherited.len(), 1);
    assert_eq!(inherited[0].delivery_mode, "notify+wake");
    assert_eq!(inherited[0].chat_type.as_deref(), Some("thread"));
    board
        .request_review(&worker, &id, &Handoff::default(), T0)
        .unwrap();
    let legs: Vec<Leg> = board
        .pending_notifications()
        .unwrap()
        .into_iter()
        .filter(|p| p.sub.task_id == id)
        .map(|p| p.leg)
        .collect();
    assert_eq!(legs, vec![Leg::Notify, Leg::Wake]);
    let pending = board.pending_notifications().unwrap();
    let wake = pending.iter().find(|p| p.leg == Leg::Wake).unwrap();
    board
        .checkpoint(&wake.sub.id, Leg::Wake, wake.event.id)
        .unwrap();
    let left: Vec<Leg> = board
        .pending_notifications()
        .unwrap()
        .into_iter()
        .map(|p| p.leg)
        .collect();
    assert_eq!(
        left,
        vec![Leg::Notify],
        "a wake checkpoint leaves the message pending"
    );
    assert!(DeliveryMode::parse("shout").is_err());
}

/// Profiles plus one worker lane, `codex-cli`, while `enabled`.
struct WithLane {
    enabled: bool,
}

impl Profiles for WithLane {
    fn exists(&self, name: &str) -> bool {
        profiles(name)
    }

    fn lane(&self, name: &str) -> bool {
        self.enabled && name == "codex-cli"
    }
}

/// Records what it was asked to start.
#[derive(Default)]
struct Recorder {
    started: Mutex<Vec<(String, bool)>>,
}

impl Spawner for Recorder {
    fn spawn(&self, request: &SpawnRequest<'_>) -> Result<(u32, u64), String> {
        self.started
            .lock()
            .unwrap()
            .push((request.profile.to_owned(), request.lane));
        Err("recorded only".to_owned())
    }
}

/// Scenario: Unknown assignee is surfaced (the lane half: a lane task is
/// handed to the lane spawner; once no plugin declares it, the task stays
/// ready with one `skipped_nonspawnable` event).
#[test]
fn a_lane_task_spawns_its_lane_and_a_vanished_lane_is_surfaced() {
    let home = Home::new();
    let mut board = home.board();
    let lanes = WithLane { enabled: true };
    let mut new = NewTask::titled("external work");
    new.assignee = Some("codex-cli".to_owned());
    let id = board.create(&user(), &new, &lanes, T0).unwrap().id;
    assert!(board.task(&id).unwrap().lane);
    let recorder = Recorder::default();
    let mut d = dispatcher(&home, &recorder, settings());
    d.profiles = &lanes;
    d.tick(T0).unwrap();
    assert_eq!(
        *recorder.started.lock().unwrap(),
        vec![("codex-cli".to_owned(), true)]
    );
    board.unblock(&user(), &id, T0 + 1).ok();
    let gone = WithLane { enabled: false };
    let mut d = dispatcher(&home, &recorder, settings());
    d.profiles = &gone;
    let task = board.task(&id).unwrap();
    if task.status != Status::Ready {
        board.move_to(&user(), &id, Status::Ready, T0 + 2).ok();
    }
    d.tick(T0 + 3).unwrap();
    d.tick(T0 + 4).unwrap();
    assert_eq!(board.task(&id).unwrap().status, Status::Ready);
    let skipped = kinds(&board, &id)
        .iter()
        .filter(|k| *k == "skipped_nonspawnable")
        .count();
    assert_eq!(skipped, 1);
    assert!(
        board.create(&user(), &new, &gone, T0).is_err(),
        "a lane nobody declares is no assignee"
    );
}

/// Collects the events it is handed and checks the board is free.
struct Watcher {
    seen: Mutex<Vec<String>>,
}

impl Observer for Watcher {
    fn observe(&self, board: &Board, events: &[Event]) {
        // Outside the transaction: the board answers reads.
        assert!(board.lease().is_ok());
        self.seen
            .lock()
            .unwrap()
            .extend(events.iter().map(|e| e.kind.clone()));
    }
}

/// The observer sees each committed transaction's events after the
/// commit.
#[test]
fn an_observer_sees_committed_events_once() {
    let home = Home::new();
    let watcher = Arc::new(Watcher {
        seen: Mutex::new(Vec::new()),
    });
    let mut board = home
        .board()
        .with_observer(Arc::clone(&watcher) as Arc<dyn Observer>);
    let id = board
        .create(&user(), &NewTask::titled("watched"), &profiles, T0)
        .unwrap()
        .id;
    board
        .block(&user(), &id, pmt_kanban::BlockKind::NeedsInput, "why", T0)
        .unwrap();
    assert_eq!(*watcher.seen.lock().unwrap(), vec!["created", "blocked"]);
}

/// A goal-mode task judged unachievable refuses completion until a person
/// unblocks it.
#[test]
fn an_unachievable_goal_refuses_every_handoff() {
    let home = Home::new();
    let mut board = home.board();
    let mut new = NewTask::titled("translate every page");
    new.goal_mode = true;
    let id = board.create(&user(), &new, &profiles, T0).unwrap().id;
    board
        .goal_verdict(
            &user(),
            &id,
            (1, 20),
            "unachievable",
            "source repo does not exist",
            T0,
        )
        .unwrap();
    let done = Completion {
        result: Some("done".to_owned()),
        ..Completion::default()
    };
    let err = board.complete(&user(), &id, &done, T0).unwrap_err();
    assert!(err.message().contains("source repo does not exist"));
    board
        .block(&user(), &id, pmt_kanban::BlockKind::NeedsInput, "judge", T0)
        .unwrap();
    board.unblock(&user(), &id, T0).unwrap();
    assert_eq!(
        board.complete(&user(), &id, &done, T0).unwrap(),
        Status::Done
    );
}
