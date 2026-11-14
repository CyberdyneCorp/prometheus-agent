//! The single dispatcher per home, its tick, caps, the allowlist, the
//! failure breaker, run tickets and worker liveness (task 67.3; spec:
//! kanban).

mod support;

use pmt_kanban::dispatch::Dispatcher;
use pmt_kanban::lease::{Holder, LEASE_MS};
use pmt_kanban::{Actor, ClaimOutcome, NewTask, Role, Status};
use support::{
    Home, Sleepers, T0, create, dispatcher, kinds, profiles, settings, sleeper, start_of, user,
};

fn other_holder() -> Holder {
    // A live process that is not us: the test runner's parent shell would
    // do, but a child we own is simpler to keep alive.
    Holder {
        kind: "daemon".into(),
        pid: std::process::id(),
        start: 1,
        host: "elsewhere".into(),
    }
}

#[test]
fn a_second_dispatcher_cannot_race_the_holder() {
    let home = Home::new();
    let id = create(&mut home.board(), "work", &[]);
    home.board().acquire_lease(&other_holder(), T0).unwrap();
    let spawner = Sleepers::default();
    let err = dispatcher(&home, &spawner, settings())
        .tick(T0 + 1_000)
        .unwrap_err();
    assert_eq!(err.exit_code(), 75);
    assert!(
        err.message()
            .starts_with("dispatcher lease held by daemon (pid"),
        "{}",
        err.message()
    );
    assert_eq!(home.board().task(&id).unwrap().status, Status::Ready);
    assert!(spawner.spawned.borrow().is_empty());
}

#[test]
fn a_dead_holders_lease_is_taken_over_after_it_expires() {
    let home = Home::new();
    home.board().acquire_lease(&other_holder(), T0).unwrap();
    let spawner = Sleepers::default();
    let d = dispatcher(&home, &spawner, settings());
    assert!(d.tick(T0 + LEASE_MS - 1).is_err());
    d.tick(T0 + LEASE_MS).unwrap();
    let board = home.board();
    let takeover = board
        .events(None)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "dispatcher_takeover")
        .unwrap();
    assert!(
        takeover.payload["previous"]
            .as_str()
            .unwrap()
            .starts_with("daemon")
    );
    assert!(board.lease().unwrap().unwrap().0.starts_with("test"));
}

#[test]
fn a_tick_claims_in_priority_order_and_reports_unassigned_tasks() {
    let home = Home::new();
    let mut board = home.board();
    let low = create(&mut board, "low", &[]);
    let mut high = NewTask::titled("high");
    high.priority = 5;
    high.assignee = Some("writer".into());
    let high = board.create(&user(), &high, &profiles, T0).unwrap().id;
    let orphan = board
        .create(&user(), &NewTask::titled("orphan"), &profiles, T0)
        .unwrap()
        .id;
    let spawner = Sleepers::default();
    let summaries = dispatcher(&home, &spawner, settings()).tick(T0).unwrap();
    let summary = &summaries[0];
    assert_eq!(summary.claimed, vec![high.clone(), low.clone()]);
    assert_eq!(summary.skipped_unassigned, vec![orphan.clone()]);
    assert_eq!(board.task(&orphan).unwrap().status, Status::Ready);
    let tick = board
        .events(None)
        .unwrap()
        .into_iter()
        .rev()
        .find(|e| e.kind == "tick")
        .unwrap();
    assert_eq!(tick.payload["skipped_unassigned"][0], orphan.as_str());
    let run = &board.runs(&high).unwrap()[0];
    assert!(run.pid.is_some() && run.lease_expires_at == Some(T0 + 900_000));
}

#[test]
fn a_default_assignee_takes_unassigned_tasks() {
    let home = Home::new();
    let orphan = home
        .board()
        .create(&user(), &NewTask::titled("orphan"), &profiles, T0)
        .unwrap()
        .id;
    let spawner = Sleepers::default();
    let mut s = settings();
    s.default_assignee = Some("ops".into());
    dispatcher(&home, &spawner, s).tick(T0).unwrap();
    assert_eq!(spawner.spawned.borrow()[0], (orphan, "ops".to_owned()));
}

#[test]
fn the_host_cap_counts_other_boards() {
    let home = Home::new();
    home.boards.create("other", None).unwrap();
    let mut other = home.boards.board("other").unwrap();
    let a = create(&mut other, "a", &[]);
    let mut b = NewTask::titled("b");
    b.assignee = Some("qa".into());
    let b = other.create(&user(), &b, &profiles, T0).unwrap().id;
    for id in [&a, &b] {
        let profile = other.task(id).unwrap().assignee.unwrap();
        other
            .claim(
                &Actor::dispatcher(),
                id,
                &profile,
                Role::Worker,
                900_000,
                T0,
            )
            .unwrap();
    }
    let here = create(&mut home.board(), "here", &[]);
    let spawner = Sleepers::default();
    let mut s = settings();
    s.max_running = 2;
    let summaries = dispatcher(&home, &spawner, s).tick(T0).unwrap();
    let default = summaries.iter().find(|s| s.board == "default").unwrap();
    assert_eq!(default.capped, vec![here]);
    assert!(spawner.spawned.borrow().is_empty());
}

#[test]
fn the_per_profile_cap_defers_extra_work() {
    let home = Home::new();
    let mut board = home.board();
    let ids: Vec<_> = (0..3)
        .map(|i| create(&mut board, &format!("t{i}"), &[]))
        .collect();
    let spawner = Sleepers::default();
    let summary = dispatcher(&home, &spawner, settings())
        .tick(T0)
        .unwrap()
        .remove(0);
    assert_eq!(summary.claimed.len(), 2);
    assert_eq!(summary.capped.len(), 1);
    let mut all: Vec<_> = summary
        .claimed
        .iter()
        .chain(&summary.capped)
        .cloned()
        .collect();
    all.sort();
    let mut ids = ids;
    ids.sort();
    assert_eq!(all, ids);
}

#[test]
fn the_claim_allowlist_fails_closed() {
    let home = Home::new();
    let id = create(&mut home.board(), "writer's", &[]);
    let spawner = Sleepers::default();
    let mut s = settings();
    s.dispatch_profiles = Some(Vec::new());
    let summary = dispatcher(&home, &spawner, s.clone())
        .tick(T0)
        .unwrap()
        .remove(0);
    assert_eq!(summary.skipped_allowlist, vec![id.clone()]);
    s.dispatch_profiles = Some(vec!["qa".into()]);
    dispatcher(&home, &spawner, s.clone()).tick(T0 + 1).unwrap();
    assert!(spawner.spawned.borrow().is_empty());
    s.dispatch_profiles = Some(vec!["writer".into()]);
    dispatcher(&home, &spawner, s).tick(T0 + 2).unwrap();
    assert_eq!(spawner.spawned.borrow().len(), 1);
}

#[test]
fn a_deleted_assignee_blocks_the_task_with_kind_capability() {
    let home = Home::new();
    let mut board = home.board();
    let id = create(&mut board, "ghost's", &[]);
    // The profile was deleted after the task was assigned.
    board.assign(&user(), &id, Some("ghost"), T0).unwrap();
    let spawner = Sleepers::default();
    dispatcher(&home, &spawner, settings()).tick(T0).unwrap();
    let task = board.task(&id).unwrap();
    assert_eq!(task.status, Status::Blocked);
    assert_eq!(task.block_kind, Some(pmt_kanban::BlockKind::Capability));
    assert_eq!(
        task.block_reason.as_deref(),
        Some("assignee profile 'ghost' does not exist")
    );
}

/// Claims `id` and records a worker that has already exited (its exit
/// code recorded when `code` is set).
fn dead_worker(board: &mut pmt_kanban::Board, id: &str, code: Option<i32>) -> String {
    let ClaimOutcome::Claimed(claim) = board
        .claim(
            &Actor::dispatcher(),
            id,
            "writer",
            Role::Worker,
            900_000,
            T0,
        )
        .unwrap()
    else {
        panic!("not claimed");
    };
    let mut child = sleeper();
    let pid = child.id();
    board
        .spawned(&claim.run_id, pid, start_of(pid), T0)
        .unwrap();
    if let Some(code) = code {
        board.worker_exit(&claim.run_id, code, None).unwrap();
    }
    child.kill().unwrap();
    child.wait().unwrap();
    claim.run_id
}

fn reap(home: &Home) -> Dispatcher<'_> {
    // A dispatcher that claims nothing new: the allowlist is empty.
    let mut s = settings();
    s.dispatch_profiles = Some(Vec::new());
    Dispatcher {
        boards: &home.boards,
        holder: Holder::current("test"),
        settings: s,
        profiles: &profiles,
        spawner: &NoSpawn,
    }
}

struct NoSpawn;

impl pmt_kanban::dispatch::Spawner for NoSpawn {
    fn spawn(&self, _: &pmt_kanban::dispatch::SpawnRequest<'_>) -> Result<(u32, u64), String> {
        Err("no spawning in this test".into())
    }
}

#[test]
fn a_dead_workers_task_is_reclaimed() {
    let home = Home::new();
    let mut board = home.board();
    let id = create(&mut board, "crashy", &[]);
    let run = dead_worker(&mut board, &id, None);
    let summary = reap(&home).tick(T0 + 1).unwrap().remove(0);
    assert_eq!(summary.reclaimed, vec![id.clone()]);
    let task = board.task(&id).unwrap();
    assert_eq!(task.status, Status::Ready);
    assert_eq!(task.consecutive_failures, 1);
    assert_eq!(board.run(&run).unwrap().outcome.as_deref(), Some("crashed"));
    // The second failure reaches the limit (2): the breaker trips.
    dead_worker(&mut board, &id, Some(1));
    reap(&home).tick(T0 + 2).unwrap();
    let task = board.task(&id).unwrap();
    assert_eq!(task.status, Status::Blocked);
    assert_eq!(
        task.last_failure.as_deref(),
        Some("worker exited with code 1")
    );
    assert!(kinds(&board, &id).contains(&"gave_up".to_owned()));
}

#[test]
fn a_terminal_exit_blocks_after_one_attempt() {
    let home = Home::new();
    let mut board = home.board();
    let id = create(&mut board, "revoked", &[]);
    dead_worker(&mut board, &id, Some(78));
    reap(&home).tick(T0 + 1).unwrap();
    let task = board.task(&id).unwrap();
    assert_eq!(task.status, Status::Blocked);
    assert_eq!(task.consecutive_failures, 1);
    assert!(kinds(&board, &id).contains(&"gave_up".to_owned()));
}

#[test]
fn a_rate_limited_exit_requeues_without_counting() {
    let home = Home::new();
    let mut board = home.board();
    let id = create(&mut board, "throttled", &[]);
    dead_worker(&mut board, &id, Some(75));
    let now = T0 + 1;
    reap(&home).tick(now).unwrap();
    let task = board.task(&id).unwrap();
    assert_eq!(task.status, Status::Ready);
    assert_eq!(task.consecutive_failures, 0);
    assert_eq!(task.not_before, Some(now + 300_000));
    let spawner = Sleepers::default();
    let summary = dispatcher(&home, &spawner, settings())
        .tick(now + 1)
        .unwrap()
        .remove(0);
    assert!(summary.claimed.is_empty(), "cooling down");
    let summary = dispatcher(&home, &spawner, settings())
        .tick(now + 300_000)
        .unwrap()
        .remove(0);
    assert_eq!(summary.claimed, vec![id]);
}

#[test]
fn a_spawn_failure_counts_but_a_cap_does_not() {
    let home = Home::new();
    let id = create(&mut home.board(), "unspawnable", &[]);
    let mut spawner = Sleepers::default();
    spawner.fail = Some("exec format error".into());
    dispatcher(&home, &spawner, settings()).tick(T0).unwrap();
    let task = home.board().task(&id).unwrap();
    assert_eq!(task.consecutive_failures, 1);
    assert_eq!(task.last_failure.as_deref(), Some("exec format error"));
}

#[test]
fn a_recycled_pid_is_reclaimed_without_being_signalled() {
    let home = Home::new();
    let mut board = home.board();
    let id = create(&mut board, "recycled", &[]);
    let ClaimOutcome::Claimed(claim) = board
        .claim(
            &Actor::dispatcher(),
            &id,
            "writer",
            Role::Worker,
            900_000,
            T0,
        )
        .unwrap()
    else {
        panic!();
    };
    let mut stranger = sleeper();
    let pid = stranger.id();
    // The recorded start time is not the stranger's: the pid was reused.
    board
        .spawned(&claim.run_id, pid, start_of(pid) - 100, T0)
        .unwrap();
    reap(&home).tick(T0 + 1).unwrap();
    assert_eq!(board.task(&id).unwrap().status, Status::Ready);
    assert!(
        stranger.try_wait().unwrap().is_none(),
        "the stranger still runs"
    );
    let _ = stranger.kill();
    let _ = stranger.wait();
}

#[test]
fn an_expired_claim_is_reclaimed_as_a_failure() {
    let home = Home::new();
    let mut board = home.board();
    let id = create(&mut board, "silent", &[]);
    let ClaimOutcome::Claimed(claim) = board
        .claim(
            &Actor::dispatcher(),
            &id,
            "writer",
            Role::Worker,
            900_000,
            T0,
        )
        .unwrap()
    else {
        panic!();
    };
    let mut child = sleeper();
    board
        .spawned(&claim.run_id, child.id(), start_of(child.id()), T0)
        .unwrap();
    board
        .heartbeat(
            &Actor::worker("writer", &claim.run_id),
            &claim.run_id,
            None,
            900_000,
            T0 + 60_000,
        )
        .unwrap();
    reap(&home).tick(T0 + 900_000).unwrap();
    assert_eq!(
        board.task(&id).unwrap().status,
        Status::Running,
        "the heartbeat renewed the lease"
    );
    reap(&home).tick(T0 + 960_000).unwrap();
    assert_eq!(
        board.run(&claim.run_id).unwrap().outcome.as_deref(),
        Some("timed_out")
    );
    assert_eq!(board.task(&id).unwrap().consecutive_failures, 1);
    assert!(
        !child.wait().unwrap().success(),
        "the silent worker was terminated"
    );
}

#[test]
fn a_worker_outliving_its_run_is_reaped() {
    let home = Home::new();
    let mut board = home.board();
    let id = create(&mut board, "lingering", &[]);
    let ClaimOutcome::Claimed(claim) = board
        .claim(
            &Actor::dispatcher(),
            &id,
            "writer",
            Role::Worker,
            900_000,
            T0,
        )
        .unwrap()
    else {
        panic!();
    };
    let mut child = sleeper();
    board
        .spawned(&claim.run_id, child.id(), start_of(child.id()), T0)
        .unwrap();
    let worker = Actor::worker("writer", &claim.run_id);
    board
        .complete(
            &worker,
            &id,
            &pmt_kanban::Completion {
                result: Some("ok".into()),
                ..Default::default()
            },
            T0,
        )
        .unwrap();
    reap(&home).tick(T0 + 119_000).unwrap();
    assert!(child.try_wait().unwrap().is_none());
    reap(&home).tick(T0 + 120_000).unwrap();
    assert!(!child.wait().unwrap().success());
    assert!(kinds(&board, &id).contains(&"worker_reaped".to_owned()));
}

#[test]
fn a_run_ticket_is_single_use() {
    let home = Home::new();
    let mut board = home.board();
    let id = create(&mut board, "ticketed", &[]);
    let ClaimOutcome::Claimed(claim) = board
        .claim(
            &Actor::dispatcher(),
            &id,
            "writer",
            Role::Worker,
            900_000,
            T0,
        )
        .unwrap()
    else {
        panic!();
    };
    assert!(board.use_ticket(&claim.run_id, "forged").is_err());
    let run = board.use_ticket(&claim.run_id, &claim.secret).unwrap();
    assert_eq!(run.task_id, id);
    let err = board.use_ticket(&claim.run_id, &claim.secret).unwrap_err();
    assert_eq!(err.message(), "run ticket already used");
    assert_eq!(err.exit_code(), 78);
}

#[test]
fn an_agent_reviewer_is_spawned_for_review_work() {
    let home = Home::new();
    let mut settings_file = home.boards.settings("default").unwrap();
    settings_file.review = true;
    home.boards
        .save_settings("default", &settings_file)
        .unwrap();
    let mut board = home.board();
    let mut new = NewTask::titled("needs eyes");
    new.assignee = Some("writer".into());
    new.reviewer = Some("qa".into());
    let id = board.create(&user(), &new, &profiles, T0).unwrap().id;
    let spawner = Sleepers::default();
    dispatcher(&home, &spawner, settings()).tick(T0).unwrap();
    let run = board.runs(&id).unwrap().remove(0);
    board
        .complete(
            &Actor::worker("writer", &run.id),
            &id,
            &pmt_kanban::Completion {
                result: Some("ok".into()),
                ..Default::default()
            },
            T0,
        )
        .unwrap();
    assert_eq!(board.task(&id).unwrap().status, Status::Review);
    dispatcher(&home, &spawner, settings())
        .tick(T0 + 1)
        .unwrap();
    let started = spawner.spawned.borrow();
    assert_eq!(started.last().unwrap(), &(id.clone(), "qa".to_owned()));
    assert_eq!(board.runs(&id).unwrap().last().unwrap().role, "reviewer");
}
