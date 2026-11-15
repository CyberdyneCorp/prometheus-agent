//! Attachments, notification subscriptions and board export/import
//! (task 67.4; spec: kanban).

mod support;

use pmt_kanban::attach::Upload;
use pmt_kanban::notify::{Leg, NewSubscription};
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
            &NewSubscription::notify("test", "c1", None, "writer"),
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
    assert_eq!(pending[0].event.kind, "completed");
    board
        .checkpoint(&sub.id, Leg::Notify, pending[0].event.id)
        .unwrap();
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
            &NewSubscription::notify("test", "c", None, "writer"),
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
        std::fs::read(copy.attachment_path(attachment).unwrap()).unwrap(),
        b"payload"
    );
    assert!(
        copy.events(Some(&child))
            .unwrap()
            .iter()
            .any(|e| e.kind == "created")
    );
}

/// An archive whose attachment row names a path instead of a hash is
/// refused before anything is created: no board, no blob.
#[test]
fn an_archive_with_a_traversal_hash_is_refused() {
    let source = Home::new();
    let mut board = source.board();
    let task = create(&mut board, "task", &[]);
    let upload = Upload {
        name: "a.txt",
        content_type: None,
        bytes: b"payload",
    };
    board.attach(&user(), &task, &upload, 1 << 20, T0).unwrap();
    let file = source.root.path().join("board.json");
    transfer::export(&source.boards, "default", &file).unwrap();
    let archive: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    for forged in ["/home/u/.ssh/id_ed25519", "../../../etc/passwd", "a", "é"] {
        let mut archive = archive.clone();
        archive["tables"]["attachments"][0]["sha256"] = forged.into();
        let forged_file = source.root.path().join("forged.json");
        std::fs::write(&forged_file, serde_json::to_vec(&archive).unwrap()).unwrap();
        let target = Home::new();
        let err = transfer::import(&target.boards, &forged_file, Some("copy")).unwrap_err();
        assert_eq!(
            err.code().code,
            "kanban.invalid_archive",
            "{forged}: {}",
            err.message()
        );
        assert!(target.boards.board("copy").is_err(), "{forged}: no board");
        let blobs = target.root.path().join("kanban/blobs");
        let stored = std::fs::read_dir(&blobs).map_or(0, Iterator::count);
        assert_eq!(stored, 0, "{forged}: nothing stored");
    }
}
