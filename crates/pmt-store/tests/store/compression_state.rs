//! The automatic-compaction guards kept with each session and pruned tool
//! output kept recoverable by its short ref (spec: context-compression,
//! Trigger decision and blocking reasons; Deterministic recoverable prune).

use jiff::Timestamp;
use pmt_store::{
    Acquire, Compaction, CompressionState, Holder, LeaseKind, NewMessage, Role, Visibility,
};

use crate::support::{TestHome, session};

#[test]
fn the_guards_survive_reopening_and_go_with_the_session() {
    let home = TestHome::new();
    let until = Timestamp::from_second(1_900_000_000).unwrap();
    let s = {
        let store = home.store("default");
        let s = session(&store);
        assert_eq!(
            store.compression_state(s.id).unwrap(),
            CompressionState::default()
        );
        let state = CompressionState {
            summary_failures: 2,
            cooldown_until: Some(until),
            backoff_until: None,
            ineffective: 1,
            pending_before: Some(120_000),
        };
        store.save_compression_state(s.id, &state).unwrap();
        s
    };
    let store = home.store("default");
    let back = store.compression_state(s.id).unwrap();
    assert_eq!(back.summary_failures, 2);
    assert_eq!(back.cooldown_until, Some(until));
    assert_eq!(back.ineffective, 1);
    assert_eq!(back.pending_before, Some(120_000));
    store.delete_session(s.id, false).unwrap();
    assert_eq!(
        store.compression_state(s.id).unwrap(),
        CompressionState::default()
    );
}

#[test]
fn a_pruned_original_is_found_by_its_short_ref_within_its_session_only() {
    let home = TestHome::new();
    let store = home.store("default");
    let s = session(&store).id;
    let other = session(&store).id;
    store
        .append_messages(
            s,
            &[
                NewMessage::new(Role::User, "run it"),
                NewMessage::new(Role::Assistant, "done"),
            ],
        )
        .unwrap();
    let read = store.messages(s, Visibility::Active).unwrap();
    let blob = store.put_blob(&vec![b'x'; 40_000]).unwrap();
    let summary = NewMessage::new(Role::User, "summary");
    let mut compaction = Compaction::new(&read, vec![read[0].id], vec![summary.clone()]);
    compaction.references.push((blob.clone(), summary.id));
    let Acquire::Granted(lock) = store
        .acquire_lease(s, LeaseKind::Compaction, &Holder::current())
        .unwrap()
    else {
        panic!("lock busy");
    };
    store.compact(&lock, &compaction).unwrap();

    let short = &blob.sha256()[..12];
    assert_eq!(store.session_blob(s, short).unwrap(), Some(blob.clone()));
    assert_eq!(store.get_blob(&blob).unwrap().len(), 40_000);
    assert_eq!(store.blob_references(&blob).unwrap(), 1);
    assert_eq!(store.session_blob(other, short).unwrap(), None);
    assert_eq!(store.session_blob(s, "zz").unwrap(), None);
}

#[test]
fn a_reference_from_a_row_the_compaction_does_not_insert_is_refused() {
    let home = TestHome::new();
    let store = home.store("default");
    let s = session(&store).id;
    store
        .append_message(s, &NewMessage::new(Role::User, "hi"))
        .unwrap();
    let read = store.messages(s, Visibility::Active).unwrap();
    let blob = store.put_blob(b"original").unwrap();
    let mut compaction = Compaction::new(
        &read,
        vec![read[0].id],
        vec![NewMessage::new(Role::User, "s")],
    );
    compaction.references.push((blob, read[0].id));
    let Acquire::Granted(lock) = store
        .acquire_lease(s, LeaseKind::Compaction, &Holder::current())
        .unwrap()
    else {
        panic!("lock busy");
    };
    let err = store.compact(&lock, &compaction).unwrap_err();
    assert_eq!(err.code().code, "store.constraint");
}
