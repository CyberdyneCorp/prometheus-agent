//! Promotion, atomic claims, typed blocks, scheduled parking, completion
//! gates, the review lane and archive (task 67.2; spec: kanban).

mod support;

use std::sync::{Arc, Barrier};

use pmt_kanban::{Actor, BlockKind, ClaimOutcome, Completion, Handoff, NewTask, Role, Status};
use support::{Home, T0, create, kinds, profiles, sleeper, start_of, user};

fn claim(board: &mut pmt_kanban::Board, id: &str) -> String {
    match board
        .claim(
            &Actor::dispatcher(),
            id,
            "writer",
            Role::Worker,
            900_000,
            T0,
        )
        .unwrap()
    {
        ClaimOutcome::Claimed(claim) => claim.run_id,
        ClaimOutcome::Rejected(open) => panic!("rejected: {open:?}"),
    }
}

fn evidence(text: &str) -> Completion {
    Completion {
        result: Some(text.to_owned()),
        attachment_limit: 1 << 20,
        ..Completion::default()
    }
}

#[test]
fn concurrent_claims_have_exactly_one_winner() {
    let home = Home::new();
    let id = create(&mut home.board(), "contended", &[]);
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let boards = home.boards.clone();
            let barrier = Arc::clone(&barrier);
            let id = id.clone();
            std::thread::spawn(move || {
                let mut board = boards.board("default").unwrap();
                barrier.wait();
                board.claim(
                    &Actor::dispatcher(),
                    &id,
                    "writer",
                    Role::Worker,
                    900_000,
                    T0,
                )
            })
        })
        .collect();
    let won = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|r| matches!(r, Ok(ClaimOutcome::Claimed(_))))
        .count();
    assert_eq!(won, 1);
    let board = home.board();
    assert_eq!(board.task(&id).unwrap().status, Status::Running);
    assert_eq!(board.runs(&id).unwrap().len(), 1);
}

#[test]
fn concurrent_archive_and_completion_settle_once() {
    let home = Home::new();
    let mut board = home.board();
    let id = create(&mut board, "race", &[]);
    let run = claim(&mut board, &id);
    let barrier = Arc::new(Barrier::new(2));
    let (b1, b2) = (home.boards.clone(), home.boards.clone());
    let (i1, i2) = (id.clone(), id.clone());
    let (w1, w2) = (Arc::clone(&barrier), Arc::clone(&barrier));
    let archive = std::thread::spawn(move || {
        let mut board = b1.board("default").unwrap();
        w1.wait();
        board.archive(&user(), &i1, T0).is_ok()
    });
    let complete = std::thread::spawn(move || {
        let mut board = b2.board("default").unwrap();
        w2.wait();
        board
            .complete(&Actor::worker("writer", &run), &i2, &evidence("done"), T0)
            .is_ok()
    });
    let (archived, completed) = (archive.join().unwrap(), complete.join().unwrap());
    let status = home.board().task(&id).unwrap().status;
    match status {
        Status::Archived => assert!(archived),
        Status::Done => assert!(completed && !archived),
        other => panic!("unexpected {other}"),
    }
}

#[test]
fn a_claim_whose_parent_reopened_is_rejected() {
    let home = Home::new();
    let mut board = home.board();
    let parent = create(&mut board, "parent", &[]);
    board
        .complete(&user(), &parent, &evidence("ok"), T0)
        .unwrap();
    let child = create(&mut board, "child", &[&parent]);
    assert_eq!(board.task(&child).unwrap().status, Status::Ready);
    board.move_to(&user(), &parent, Status::Ready, T0).unwrap();
    let outcome = board
        .claim(
            &Actor::dispatcher(),
            &child,
            "writer",
            Role::Worker,
            900_000,
            T0,
        )
        .unwrap();
    assert!(matches!(outcome, ClaimOutcome::Rejected(open) if open == vec![parent.clone()]));
    assert_eq!(board.task(&child).unwrap().status, Status::Todo);
    assert!(kinds(&board, &child).contains(&"claim_rejected".to_owned()));
}

#[test]
fn a_worker_block_is_sticky() {
    let home = Home::new();
    let mut board = home.board();
    let id = create(&mut board, "needs creds", &[]);
    let run = claim(&mut board, &id);
    let to = board
        .block(
            &Actor::worker("writer", &run),
            &id,
            BlockKind::Capability,
            "no API key",
            T0,
        )
        .unwrap();
    assert_eq!(to, Status::Blocked);
    assert_eq!(
        board.runs(&id).unwrap()[0].outcome.as_deref(),
        Some("blocked")
    );
    board.promote_all(&Actor::dispatcher(), T0 + 1).unwrap();
    assert_eq!(board.task(&id).unwrap().status, Status::Blocked);
    let mut new = NewTask::titled("held");
    new.initial_status = Some(Status::Blocked);
    let held = board.create(&user(), &new, &profiles, T0).unwrap().id;
    board.promote_all(&Actor::dispatcher(), T0 + 1).unwrap();
    assert_eq!(board.task(&held).unwrap().status, Status::Blocked);
}

#[test]
fn a_repeated_same_cause_block_escalates_to_triage() {
    let home = Home::new();
    let mut board = home.board();
    let id = create(&mut board, "question", &[]);
    board
        .block(&user(), &id, BlockKind::NeedsInput, "which region?", T0)
        .unwrap();
    assert_eq!(board.unblock(&user(), &id, T0).unwrap(), Status::Ready);
    let to = board
        .block(
            &user(),
            &id,
            BlockKind::NeedsInput,
            "which region again?",
            T0,
        )
        .unwrap();
    assert_eq!(to, Status::Triage);
    assert!(kinds(&board, &id).contains(&"block_loop_detected".to_owned()));
    // Another cause is not a loop.
    let other = create(&mut board, "other", &[]);
    board
        .block(&user(), &other, BlockKind::Transient, "flaky", T0)
        .unwrap();
    board.unblock(&user(), &other, T0).unwrap();
    assert_eq!(
        board
            .block(&user(), &other, BlockKind::NeedsInput, "q", T0)
            .unwrap(),
        Status::Blocked
    );
}

#[test]
fn dependency_blocks_wait_for_parents_and_review_blocks_return_to_review() {
    let home = Home::new();
    let mut board = home.board();
    let parent = create(&mut board, "parent", &[]);
    let id = create(&mut board, "child", &[]);
    board.link(&user(), &parent, &id, None, T0).unwrap();
    board.promote_all(&Actor::dispatcher(), T0).unwrap();
    let lone = create(&mut board, "lone", &[]);
    let run = claim(&mut board, &lone);
    let to = board
        .block(
            &Actor::worker("writer", &run),
            &lone,
            BlockKind::Dependency,
            "waiting on infra",
            T0,
        )
        .unwrap();
    assert_eq!(to, Status::Blocked, "no open parent: re-kinded needs_input");
    assert_eq!(
        board.task(&lone).unwrap().block_kind,
        Some(BlockKind::NeedsInput)
    );
    let reviewed = create(&mut board, "reviewed", &[]);
    board
        .request_review(&user(), &reviewed, &Handoff::default(), T0)
        .unwrap();
    board
        .block(&user(), &reviewed, BlockKind::NeedsInput, "ask legal", T0)
        .unwrap();
    assert_eq!(
        board.unblock(&user(), &reviewed, T0).unwrap(),
        Status::Review
    );
}

#[test]
fn a_scheduled_task_wakes_at_its_time_and_is_never_claimed_before() {
    let home = Home::new();
    let mut board = home.board();
    let id = create(&mut board, "morning", &[]);
    let wake = T0 + 8 * 3_600_000;
    board.schedule(&user(), &id, Some(wake), T0).unwrap();
    assert!(
        board
            .claim(
                &Actor::dispatcher(),
                &id,
                "writer",
                Role::Worker,
                900_000,
                T0
            )
            .is_err()
    );
    board.promote_all(&Actor::dispatcher(), wake - 1).unwrap();
    assert_eq!(board.task(&id).unwrap().status, Status::Scheduled);
    board.promote_all(&Actor::dispatcher(), wake).unwrap();
    assert_eq!(board.task(&id).unwrap().status, Status::Ready);
    assert!(kinds(&board, &id).contains(&"woke".to_owned()));
    let running = create(&mut board, "parked", &[]);
    claim(&mut board, &running);
    board.schedule(&user(), &running, None, T0).unwrap();
    assert_eq!(
        board.runs(&running).unwrap()[0].outcome.as_deref(),
        Some("scheduled")
    );
    assert_eq!(board.unblock(&user(), &running, T0).unwrap(), Status::Ready);
}

#[test]
fn whitespace_evidence_is_refused_and_recorded() {
    let home = Home::new();
    let mut board = home.board();
    let id = create(&mut board, "report", &[]);
    let run = claim(&mut board, &id);
    let err = board
        .complete(&Actor::worker("writer", &run), &id, &evidence("   "), T0)
        .unwrap_err();
    assert_eq!(
        err.message(),
        format!("completion blocked: {id} has no result or summary evidence")
    );
    assert_eq!(board.task(&id).unwrap().status, Status::Running);
    let refused = board
        .events(Some(&id))
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "completion_refused")
        .unwrap();
    assert_eq!(refused.payload["reason"], err.message());
}

#[test]
fn fabricated_child_cards_are_refused() {
    let home = Home::new();
    let mut board = home.board();
    let id = create(&mut board, "plan", &[]);
    let run = claim(&mut board, &id);
    let worker = Actor::worker("writer", &run);
    let real = board
        .create(&worker, &NewTask::titled("follow-up"), &profiles, T0)
        .unwrap()
        .id;
    let mut c = evidence("planned");
    c.created_tasks = vec!["t_deadbeef".into()];
    assert!(board.complete(&worker, &id, &c, T0).is_err());
    assert!(kinds(&board, &id).contains(&"hallucinated_cards".to_owned()));
    c.created_tasks = vec![real];
    assert_eq!(board.complete(&worker, &id, &c, T0).unwrap(), Status::Done);
}

#[test]
fn a_live_worker_holds_its_task_against_a_plain_completion() {
    let home = Home::new();
    let mut board = home.board();
    let id = create(&mut board, "busy", &[]);
    let run = claim(&mut board, &id);
    let mut child = sleeper();
    board
        .spawned(&run, child.id(), start_of(child.id()), T0)
        .unwrap();
    let err = board
        .complete(&user(), &id, &evidence("manual"), T0)
        .unwrap_err();
    assert_eq!(
        err.message(),
        format!("task {id} is running under a live worker; pass the run id or --force")
    );
    let mut c = evidence("manual");
    c.run_id = Some(run);
    assert_eq!(board.complete(&user(), &id, &c, T0).unwrap(), Status::Done);
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn a_missing_declared_artifact_keeps_the_task_in_flight() {
    let home = Home::new();
    let mut board = home.board();
    let mut new = NewTask::titled("quarterly");
    new.artifacts = vec!["report.pdf".into()];
    let id = board.create(&user(), &new, &profiles, T0).unwrap().id;
    let run = claim(&mut board, &id);
    let worker = Actor::worker("writer", &run);
    let err = board
        .complete(&worker, &id, &evidence("done"), T0)
        .unwrap_err();
    assert_eq!(err.message(), "declared artifact missing: report.pdf");
    assert_eq!(board.task(&id).unwrap().status, Status::Running);
    let scratch = board.scratch_dir(&id);
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::write(scratch.join("report.pdf"), b"%PDF-1.7").unwrap();
    assert_eq!(
        board.complete(&worker, &id, &evidence("done"), T0).unwrap(),
        Status::Done
    );
    let attachments = board.attachments(&id).unwrap();
    assert_eq!(attachments[0].filename, "report.pdf");
    assert_eq!(
        std::fs::read(board.attachment_path(&attachments[0]).unwrap()).unwrap(),
        b"%PDF-1.7"
    );
    assert!(!scratch.exists(), "a done scratch workspace is deleted");
}

#[test]
fn the_review_lane_takes_worker_completions() {
    let home = Home::new();
    let mut settings = home.boards.settings("default").unwrap();
    settings.review = true;
    home.boards.save_settings("default", &settings).unwrap();
    let mut board = home.board();
    let id = create(&mut board, "reviewed work", &[]);
    let run = claim(&mut board, &id);
    assert_eq!(
        board
            .complete(&Actor::worker("writer", &run), &id, &evidence("diff"), T0)
            .unwrap(),
        Status::Review
    );
    assert!(kinds(&board, &id).contains(&"review_requested".to_owned()));
    board
        .approve(&Actor::reviewer("qa"), &id, Some("lgtm"), T0)
        .unwrap();
    assert_eq!(board.task(&id).unwrap().status, Status::Done);
}

#[test]
fn archiving_a_running_task_terminates_its_worker() {
    let home = Home::new();
    let mut board = home.board();
    let id = create(&mut board, "doomed", &[]);
    let run = claim(&mut board, &id);
    let mut child = sleeper();
    let pid = child.id();
    board.spawned(&run, pid, start_of(pid), T0).unwrap();
    board.archive(&user(), &id, T0).unwrap();
    assert_eq!(board.task(&id).unwrap().status, Status::Archived);
    let status = child.wait().unwrap();
    assert!(!status.success());
    let event = board
        .events(Some(&id))
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "archive_worker_termination")
        .unwrap();
    assert_eq!(event.payload["signal"], "signalled");
    assert!(board.subscriptions(Some(&id)).unwrap().is_empty());
}
