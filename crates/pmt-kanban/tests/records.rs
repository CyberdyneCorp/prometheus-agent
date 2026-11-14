//! Attachments, notification subscriptions and board export/import
//! (task 67.4; spec: kanban).

mod support;

use pmt_kanban::attach::Upload;
use pmt_kanban::notify::NewSubscription;
use pmt_kanban::{Actor, Completion, transfer};
use support::{Home, T0, create, user};

#[test]
fn an_oversized_attachment_stores_nothing() {
    let home = Home::new();
    let mut board = home.board();
    let id = create(&mut board, "big", &[]);
    let bytes = vec![0_u8; 1025];
    let upload = Upload {
        name: "../../etc/report.bin",
        content_type: None,
        bytes: &bytes,
    };
    let err = board.attach(&user(), &id, &upload, 1024, T0).unwrap_err();
    assert_eq!(err.message(), "attachment too large: 1025 > 1024");
    assert!(board.attachments(&id).unwrap().is_empty());
    assert!(std::fs::read_dir(board.blobs()).unwrap().next().is_none());
    let stored = board.attach(&user(), &id, &upload, 2048, T0).unwrap();
    assert_eq!(stored.filename, "report.bin", "directories are stripped");
}

#[test]
fn subscriptions_report_each_event_once_and_drop_on_archive() {
    let home = Home::new();
    let mut board = home.board();
    let id = create(&mut board, "watched", &[]);
    let sub = board
        .subscribe(
            &user(),
            &id,
            &NewSubscription {
                adapter: "test".into(),
                chat: "c1".into(),
                thread: None,
                profile: "writer".into(),
            },
            T0,
        )
        .unwrap();
    board
        .complete(
            &user(),
            &id,
            &Completion {
                result: Some("ok".into()),
                ..Default::default()
            },
            T0,
        )
        .unwrap();
    let pending = board.pending_notifications().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].1.kind, "completed");
    board.checkpoint(&sub.id, pending[0].1.id).unwrap();
    assert!(board.pending_notifications().unwrap().is_empty());
    assert!(board.warn_once(&sub.id).unwrap());
    assert!(
        !board.warn_once(&sub.id).unwrap(),
        "one warning per subscription"
    );
    assert_eq!(board.purge_done_subscriptions(1000, T0 + 999).unwrap(), 0);
    assert_eq!(board.purge_done_subscriptions(1000, T0 + 1000).unwrap(), 1);
    let other = create(&mut board, "other", &[]);
    board
        .subscribe(
            &user(),
            &other,
            &NewSubscription {
                adapter: "test".into(),
                chat: "c".into(),
                thread: None,
                profile: "writer".into(),
            },
            T0,
        )
        .unwrap();
    board.archive(&user(), &other, T0).unwrap();
    assert!(board.subscriptions(Some(&other)).unwrap().is_empty());
}

#[test]
fn a_board_imported_into_another_home_keeps_its_ids() {
    let source = Home::new();
    let mut board = source.board();
    let parent = create(&mut board, "parent", &[]);
    let child = create(&mut board, "child", &[&parent]);
    board
        .comment(&Actor::user("cli"), &child, "note", T0)
        .unwrap();
    let upload = Upload {
        name: "a.txt",
        content_type: None,
        bytes: b"payload",
    };
    board.attach(&user(), &child, &upload, 1 << 20, T0).unwrap();
    let file = source.root.path().join("board.json");
    assert_eq!(
        transfer::export(&source.boards, "default", &file).unwrap(),
        2
    );
    let target = Home::new();
    let err = transfer::import(&target.boards, &file, None).unwrap_err();
    assert!(
        err.message().contains("already exists"),
        "{}",
        err.message()
    );
    let imported = transfer::import(&target.boards, &file, Some("copy")).unwrap();
    assert!(imported.remapped.is_empty());
    let copy = target.boards.board("copy").unwrap();
    assert_eq!(copy.parents(&child).unwrap(), vec![parent]);
    assert_eq!(copy.comments(&child).unwrap()[0].body, "note");
    let attachment = &copy.attachments(&child).unwrap()[0];
    assert_eq!(
        std::fs::read(copy.attachment_path(attachment)).unwrap(),
        b"payload"
    );
    assert!(
        copy.events(Some(&child))
            .unwrap()
            .iter()
            .any(|e| e.kind == "created")
    );
}
