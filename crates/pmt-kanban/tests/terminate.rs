//! A person terminating a run (spec: web-dashboard, Kanban dashboard API
//! and orchestration modes, "Terminate a stuck run"): the worker is
//! stopped and its task returns to `ready` without counting a failure.

mod support;

use pmt_kanban::{Actor, Boards, Status};
use support::{Home, Sleepers, T0, create, dispatcher, kinds, settings};

fn breaker() -> pmt_kanban::Breaker {
    settings().breaker
}

#[test]
fn terminating_a_running_worker_frees_its_task() {
    let home = Home::new();
    let id = create(&mut home.board(), "stuck", &[]);
    let spawner = Sleepers::default();
    dispatcher(&home, &spawner, settings()).tick(T0).unwrap();
    let mut board = home.board();
    let run = board.runs(&id).unwrap().remove(0);
    assert_eq!(board.task(&id).unwrap().status, Status::Running);
    let found = home.boards.board_of_run(&run.id).unwrap();
    assert_eq!(found.slug(), "default");

    let stopped = board
        .terminate(&Actor::user("dashboard"), &run.id, breaker(), T0 + 5_000)
        .unwrap();

    assert_eq!(stopped.task_id, id);
    assert_eq!(stopped.status, Status::Ready);
    assert_eq!(
        stopped.signal,
        Some(pmt_kanban::process::Terminated::Signalled)
    );
    let task = board.task(&id).unwrap();
    assert_eq!(task.status, Status::Ready);
    assert_eq!(task.consecutive_failures, 0);
    let ended = board.run(&run.id).unwrap();
    assert_eq!(ended.outcome.as_deref(), Some("reclaimed"));
    assert!(kinds(&board, &id).contains(&"terminated".to_owned()));
    // The sleeper got the signal: it exits rather than running out its 30 s.
    let status = spawner.children.borrow_mut()[0].wait().unwrap();
    assert!(!status.success(), "{status:?}");
}

#[test]
fn an_ended_or_unknown_run_is_refused() {
    let home = Home::new();
    let id = create(&mut home.board(), "stuck", &[]);
    let spawner = Sleepers::default();
    dispatcher(&home, &spawner, settings()).tick(T0).unwrap();
    let mut board = home.board();
    let run = board.runs(&id).unwrap().remove(0);
    let user = Actor::user("dashboard");
    board.terminate(&user, &run.id, breaker(), T0 + 1).unwrap();

    let again = board
        .terminate(&user, &run.id, breaker(), T0 + 2)
        .unwrap_err();
    assert_eq!(again.code().code, "kanban.illegal_transition");
    let unknown = board.terminate(&user, "r_nope", breaker(), T0).unwrap_err();
    assert_eq!(unknown.code().code, "kanban.run_not_found");
    let nowhere = Boards::open(home.root.path())
        .unwrap()
        .board_of_run("r_nope")
        .unwrap_err();
    assert_eq!(nowhere.code().code, "kanban.run_not_found");
}
