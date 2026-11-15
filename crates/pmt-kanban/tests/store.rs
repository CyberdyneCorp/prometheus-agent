//! Board storage, selection, the workflow table, creation and links
//! (task 67.1; spec: kanban).

mod support;

use pmt_kanban::{Actor, Handoff, NewTask, Status, Workspace};
use support::{Home, T0, create, kinds, profiles, user};

#[test]
fn the_default_board_exists_on_first_use() {
    let home = Home::new();
    let db = home.root.path().join("kanban/boards/default/board.db");
    assert!(db.is_file());
    assert!(
        home.root
            .path()
            .join("kanban/boards/default/board.toml")
            .is_file()
    );
    assert!(
        home.root
            .path()
            .join("kanban/boards/default/workspaces")
            .is_dir()
    );
    assert!(home.root.path().join("kanban/boards/default/logs").is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&db).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    assert!(
        home.board()
            .tasks(&pmt_kanban::Filter::default())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_stale_selection_is_reported_never_replaced() {
    let home = Home::new();
    let err = home.boards.resolve(None, Some("atm10")).unwrap_err();
    assert_eq!(err.exit_code(), 66);
    assert_eq!(
        err.message(),
        "board 'atm10' not found (set kanban.current_board or pass --board)"
    );
    let err = home.boards.resolve(Some("nope"), None).unwrap_err();
    assert_eq!(err.message(), "board 'nope' not found");
    assert_eq!(home.boards.resolve(None, None).unwrap(), "default");
    home.boards.create("Ops", None).unwrap();
    assert_eq!(
        home.boards.resolve(Some("OPS"), Some("atm10")).unwrap(),
        "ops"
    );
    home.boards.set_archived("ops", true).unwrap();
    assert_eq!(
        home.boards
            .resolve(Some("ops"), None)
            .unwrap_err()
            .exit_code(),
        66
    );
    let err = home.boards.create("_bad", None).unwrap_err();
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn request_changes_lands_in_todo_and_is_promoted() {
    let home = Home::new();
    let mut board = home.board();
    let parent = create(&mut board, "parent", &[]);
    let id = create(&mut board, "child", &[&parent]);
    board
        .complete(
            &user(),
            &parent,
            &pmt_kanban::Completion {
                result: Some("ok".into()),
                ..Default::default()
            },
            T0,
        )
        .unwrap();
    board.promote_all(&Actor::dispatcher(), T0).unwrap();
    board
        .request_review(
            &user(),
            &id,
            &Handoff {
                summary: Some("look".into()),
                ..Handoff::default()
            },
            T0,
        )
        .unwrap();
    board
        .request_changes(&Actor::reviewer("qa"), &id, "rename the module", T0)
        .unwrap();
    assert_eq!(board.task(&id).unwrap().status, Status::Todo);
    assert_eq!(board.comments(&id).unwrap()[0].body, "rename the module");
    let promoted = board.promote_all(&Actor::dispatcher(), T0).unwrap();
    assert_eq!(promoted, vec![id.clone()]);
    assert_eq!(board.task(&id).unwrap().status, Status::Ready);
}

#[test]
fn a_manual_move_outside_the_table_is_refused() {
    let home = Home::new();
    let mut board = home.board();
    let mut new = NewTask::titled("idea");
    new.initial_status = Some(Status::Triage);
    let id = board.create(&user(), &new, &profiles, T0).unwrap().id;
    let err = board
        .move_to(&user(), &id, Status::Running, T0)
        .unwrap_err();
    assert_eq!(
        err.message(),
        "illegal transition triage → running for user"
    );
    assert_eq!(err.code().code, "kanban.illegal_transition");
    board.move_to(&user(), &id, Status::Todo, T0).unwrap();
    assert_eq!(board.task(&id).unwrap().status, Status::Todo);
}

#[test]
fn a_child_of_an_open_parent_waits() {
    let home = Home::new();
    let mut board = home.board();
    let parent = create(&mut board, "parent", &[]);
    board
        .claim(
            &Actor::dispatcher(),
            &parent,
            "writer",
            pmt_kanban::Role::Worker,
            900_000,
            T0,
        )
        .unwrap();
    let child = create(&mut board, "child", &[&parent]);
    let task = board.task(&child).unwrap();
    assert_eq!(task.status, Status::Todo);
    assert!(task.id.starts_with("t_") && task.id.len() == 10);
    let wait = board
        .events(Some(&child))
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "dependency_wait")
        .unwrap();
    assert_eq!(wait.payload["parents"][0], parent.as_str());
    let free = create(&mut board, "free", &[]);
    assert_eq!(board.task(&free).unwrap().status, Status::Ready);
}

#[test]
fn creation_validates_before_writing_anything() {
    let home = Home::new();
    let mut board = home.board();
    let err = Workspace::parse("dir:../tenants/foo").unwrap_err();
    assert_eq!(err.message(), "workspace dir must be an absolute path");
    let mut new = NewTask::titled("   ");
    assert_eq!(
        board
            .create(&user(), &new, &profiles, T0)
            .unwrap_err()
            .message(),
        "title is required"
    );
    new.title = "x".into();
    new.parents = vec!["t_00000000".into(), "t_11111111".into()];
    assert_eq!(
        board
            .create(&user(), &new, &profiles, T0)
            .unwrap_err()
            .message(),
        "unknown parent task(s): t_00000000, t_11111111"
    );
    new.parents.clear();
    new.assignee = Some("ghost".into());
    let err = board.create(&user(), &new, &profiles, T0).unwrap_err();
    assert_eq!(err.message(), "unknown profile 'ghost'");
    assert!(
        board
            .tasks(&pmt_kanban::Filter::default())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        Workspace::parse("worktree:feat/x").unwrap(),
        Workspace::Worktree {
            branch: Some("feat/x".into())
        }
    );
    assert!(matches!(
        Workspace::parse("dir:/srv/tenants/foo").unwrap(),
        Workspace::Dir { .. }
    ));
}

#[test]
fn a_retried_create_returns_the_same_task() {
    let home = Home::new();
    let mut board = home.board();
    let mut new = NewTask::titled("nightly report");
    new.idempotency_key = Some("nightly-2026-10-29".into());
    let first = board.create(&user(), &new, &profiles, T0).unwrap();
    let again = board.create(&user(), &new, &profiles, T0 + 1).unwrap();
    assert!(first.created);
    assert!(!again.created);
    assert_eq!(first.id, again.id);
    let tasks = board.tasks(&pmt_kanban::Filter::default()).unwrap();
    assert_eq!(tasks.len(), 1);
    board.archive(&user(), &first.id, T0).unwrap();
    assert!(
        board
            .create(&user(), &new, &profiles, T0 + 2)
            .unwrap()
            .created
    );
}

#[test]
fn links_refuse_cycles_self_links_and_running_children() {
    let home = Home::new();
    let mut board = home.board();
    let a = create(&mut board, "A", &[]);
    let b = create(&mut board, "B", &[]);
    board.link(&user(), &b, &a, None, T0).unwrap();
    assert_eq!(
        board.task(&a).unwrap().status,
        Status::Todo,
        "a ready child of an open parent is demoted"
    );
    let err = board.link(&user(), &a, &b, None, T0).unwrap_err();
    assert_eq!(
        err.message(),
        format!("linking {a} → {b} would create a cycle")
    );
    assert_eq!(board.parents(&b).unwrap(), Vec::<String>::new());
    assert_eq!(
        board.link(&user(), &a, &a, None, T0).unwrap_err().message(),
        "a task cannot depend on itself"
    );
    let c = create(&mut board, "C", &[]);
    let run = match board
        .claim(
            &Actor::dispatcher(),
            &c,
            "writer",
            pmt_kanban::Role::Worker,
            900_000,
            T0,
        )
        .unwrap()
    {
        pmt_kanban::ClaimOutcome::Claimed(claim) => claim.run_id,
        pmt_kanban::ClaimOutcome::Rejected(_) => unreachable!(),
    };
    assert!(board.link(&user(), &b, &c, None, T0).is_err());
    board.link(&user(), &b, &c, Some(&run), T0).unwrap();
    let done = create(&mut board, "done", &[]);
    board
        .complete(
            &user(),
            &done,
            &pmt_kanban::Completion {
                result: Some("ok".into()),
                ..Default::default()
            },
            T0,
        )
        .unwrap();
    let d = create(&mut board, "D", &[]);
    board.link(&user(), &done, &d, None, T0).unwrap();
    assert_eq!(
        board.task(&d).unwrap().status,
        Status::Ready,
        "a done parent never gates"
    );
    assert!(kinds(&board, &d).contains(&"linked".to_owned()));
}
