//! In-place compaction, the turn lease and the compaction lock (spec:
//! session-storage, Non-destructive in-place compaction; Turn lease and
//! compaction lock; context-compression, One compaction at a time per
//! session; Compaction is always in place).

use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use pmt_core::id::{MessageId, SessionId};
use pmt_store::{
    Acquire, Compaction, Holder, Lease, LeaseKind, Message, NewMessage, Role, Store, Visibility,
};

use crate::support::{TestHome, raw, session};

fn filled(store: &Store, count: usize) -> SessionId {
    let chat = session(store).id;
    let rows: Vec<NewMessage> = (1..=count)
        .map(|i| {
            let role = if i % 2 == 1 {
                Role::User
            } else {
                Role::Assistant
            };
            NewMessage::new(role, format!("message {i}"))
        })
        .collect();
    store.append_messages(chat, &rows).unwrap();
    chat
}

fn lock(store: &Store, chat: SessionId) -> Lease {
    match store
        .acquire_lease(chat, LeaseKind::Compaction, &Holder::current())
        .unwrap()
    {
        Acquire::Granted(lease) => lease,
        Acquire::Busy { holder, .. } => panic!("held by {holder:?}"),
    }
}

fn summary() -> NewMessage {
    let mut row = NewMessage::new(Role::User, "[CONTEXT COMPACTION - REFERENCE ONLY] summary");
    row.metadata = Some(serde_json::json!({"compaction_summary": true}));
    row
}

/// Summarises the active rows `range` (1-based, inclusive) of `read`.
fn plan(read: &[Message], range: std::ops::RangeInclusive<usize>) -> Compaction {
    let replaced: Vec<MessageId> = read[range.start() - 1..*range.end()]
        .iter()
        .map(|m| m.id)
        .collect();
    Compaction::new(read, replaced, vec![summary()])
}

fn contents(store: &Store, chat: SessionId) -> Vec<String> {
    store
        .messages(chat, Visibility::Active)
        .unwrap()
        .into_iter()
        .map(|m| m.content.unwrap_or_default())
        .collect()
}

#[test]
fn compaction_keeps_the_session_and_the_originals() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = filled(&store, 10);
    let read = store.messages(chat, Visibility::Active).unwrap();
    let lease = lock(&store, chat);

    let done = store.compact(&lease, &plan(&read, 2..=8)).unwrap();

    assert_eq!(done.epoch, 1);
    assert_eq!(done.inserted, [9]);
    let active = contents(&store, chat);
    assert_eq!(active[0], "message 1");
    assert!(active[1].starts_with("[CONTEXT COMPACTION"));
    assert_eq!(&active[2..], ["message 9", "message 10"]);
    let all = store.messages(chat, Visibility::All).unwrap();
    let archived: Vec<&Message> = all.iter().filter(|m| m.compacted).collect();
    assert_eq!(archived.len(), 7, "originals stay recoverable");
    assert!(
        archived
            .iter()
            .all(|m| !m.active && m.compaction_epoch == Some(1))
    );
    assert_eq!(archived[0].content.as_deref(), Some("message 2"));
    let seqs: Vec<u64> = all.iter().map(|m| m.seq).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    assert_eq!(store.session(chat).unwrap().compaction_count, 1);
}

#[test]
fn append_during_compaction_is_sequenced_after_the_summary() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = filled(&store, 40);
    let read = store.messages(chat, Visibility::Active).unwrap();
    let lease = lock(&store, chat);
    store
        .append_message(chat, &NewMessage::new(Role::User, "message 41"))
        .unwrap();

    store.compact(&lease, &plan(&read, 1..=40)).unwrap();

    let active = store.messages(chat, Visibility::Active).unwrap();
    assert_eq!(active.len(), 2);
    assert!(
        active[0]
            .content
            .as_deref()
            .unwrap()
            .starts_with("[CONTEXT")
    );
    assert_eq!(active[1].content.as_deref(), Some("message 41"));
    assert!(active[0].seq < active[1].seq);
}

#[test]
fn rewritten_source_row_aborts_the_compaction() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = filled(&store, 20);
    let read = store.messages(chat, Visibility::Active).unwrap();
    let lease = lock(&store, chat);
    // Message 12 is rewound (soft-deleted) by another writer meanwhile.
    raw(&home.database("default"))
        .execute(
            "UPDATE messages SET active = 0 WHERE id = ?1",
            [read[11].id.to_string()],
        )
        .unwrap();
    let before = store.messages(chat, Visibility::All).unwrap();

    let err = store.compact(&lease, &plan(&read, 2..=15)).unwrap_err();

    assert_eq!(err.code().code, "store.compaction_conflict");
    assert!(err.message().contains(&read[11].id.to_string()), "{err}");
    assert_eq!(
        store.messages(chat, Visibility::All).unwrap(),
        before,
        "no rows changed"
    );
    assert_eq!(store.session(chat).unwrap().compaction_count, 0);
}

#[test]
fn compaction_without_the_lock_is_refused() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = filled(&store, 6);
    let read = store.messages(chat, Visibility::Active).unwrap();
    let lease = lock(&store, chat);
    store.release_lease(&lease).unwrap();
    let err = store.compact(&lease, &plan(&read, 1..=4)).unwrap_err();
    assert_eq!(err.code().code, "store.lease_lost");
}

#[test]
fn two_compactions_on_one_session_serialize() {
    let home = TestHome::new();
    let chat = filled(&home.store("default"), 30);
    let (first, second) = (home.store("default"), home.store("default"));
    let first_holds = Arc::new(Barrier::new(2));
    let second_tried = Arc::new(Barrier::new(2));
    std::thread::scope(|threads| {
        let (holds, tried) = (Arc::clone(&first_holds), Arc::clone(&second_tried));
        let first = &first;
        threads.spawn(move || {
            let read = first.messages(chat, Visibility::Active).unwrap();
            let lease = lock(first, chat);
            holds.wait();
            tried.wait();
            let done = first.compact(&lease, &plan(&read, 1..=10)).unwrap();
            first.release_lease(&lease).unwrap();
            assert_eq!(done.epoch, 1);
        });
        first_holds.wait();
        let busy = second
            .acquire_lease(chat, LeaseKind::Compaction, &Holder::current())
            .unwrap();
        assert!(matches!(busy, Acquire::Busy { .. }), "lock_skipped");
        second_tried.wait();
    });
    let read = second.messages(chat, Visibility::Active).unwrap();
    let lease = lock(&second, chat);
    let done = second.compact(&lease, &plan(&read, 1..=5)).unwrap();
    assert_eq!(done.epoch, 2);
}

#[test]
fn racing_compactors_never_overlap() {
    let home = TestHome::new();
    let chat = filled(&home.store("default"), 60);
    let workers = 4;
    let start = Arc::new(Barrier::new(workers));
    let epochs: Vec<u32> = std::thread::scope(|threads| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                let (store, start) = (home.store("default"), Arc::clone(&start));
                threads.spawn(move || {
                    start.wait();
                    loop {
                        let lease = match store
                            .acquire_lease(chat, LeaseKind::Compaction, &Holder::current())
                            .unwrap()
                        {
                            Acquire::Granted(lease) => lease,
                            Acquire::Busy { .. } => {
                                std::thread::sleep(Duration::from_millis(2));
                                continue;
                            }
                        };
                        let read = store.messages(chat, Visibility::Active).unwrap();
                        let done = store.compact(&lease, &plan(&read, 1..=5)).unwrap();
                        store.release_lease(&lease).unwrap();
                        return done.epoch;
                    }
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut sorted = epochs.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, [1, 2, 3, 4], "{epochs:?}");
    let all = home
        .store("default")
        .messages(chat, Visibility::All)
        .unwrap();
    assert_eq!(all.iter().filter(|m| m.compacted).count(), 20);
}

#[test]
fn held_turn_lease_reports_its_holder() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store).id;
    let me = Holder::current();
    let Acquire::Granted(mut lease) = store.acquire_lease(chat, LeaseKind::Turn, &me).unwrap()
    else {
        panic!("free lease");
    };
    assert_eq!(
        store.acquire_lease(chat, LeaseKind::Turn, &me).unwrap(),
        Acquire::Busy {
            holder: me.clone(),
            expires_at: lease.expires_at()
        }
    );
    // The compaction lock is a separate lease.
    assert!(matches!(
        store
            .acquire_lease(chat, LeaseKind::Compaction, &me)
            .unwrap(),
        Acquire::Granted(_)
    ));
    let before = lease.expires_at();
    std::thread::sleep(Duration::from_millis(5));
    store.renew_lease(&mut lease).unwrap();
    assert!(lease.expires_at() > before);
    store.release_lease(&lease).unwrap();
    assert!(matches!(
        store.acquire_lease(chat, LeaseKind::Turn, &me).unwrap(),
        Acquire::Granted(_)
    ));
}

#[test]
fn dead_holder_is_reclaimed_without_waiting_for_expiry() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store).id;
    let mut exited = Command::new(std::env::current_exe().unwrap())
        .arg("--list")
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let dead = Holder {
        pid: exited.id(),
        ..Holder::current()
    };
    exited.wait().unwrap();
    let granted = store.acquire_lease(chat, LeaseKind::Turn, &dead).unwrap();
    assert!(matches!(granted, Acquire::Granted(_)));

    let taken = store
        .acquire_lease(chat, LeaseKind::Turn, &Holder::current())
        .unwrap();
    assert!(matches!(taken, Acquire::Granted(_)), "{taken:?}");
}

#[test]
fn remote_holder_is_trusted_until_expiry() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store).id;
    let remote = Holder {
        pid: 1,
        process_start: 1,
        host_id: "another-host".to_owned(),
    };
    let ttl = Duration::from_millis(300);
    assert!(matches!(
        store
            .acquire_lease_for(chat, LeaseKind::Turn, &remote, ttl)
            .unwrap(),
        Acquire::Granted(_)
    ));
    let me = Holder::current();
    assert!(matches!(
        store.acquire_lease(chat, LeaseKind::Turn, &me).unwrap(),
        Acquire::Busy { .. }
    ));
    std::thread::sleep(ttl + Duration::from_millis(50));
    assert!(matches!(
        store.acquire_lease(chat, LeaseKind::Turn, &me).unwrap(),
        Acquire::Granted(_)
    ));
}

#[test]
fn renewing_a_reclaimed_lease_fails() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store).id;
    let me = Holder::current();
    let short = Duration::from_millis(50);
    let Acquire::Granted(mut lease) = store
        .acquire_lease_for(chat, LeaseKind::Turn, &me, short)
        .unwrap()
    else {
        panic!("free lease");
    };
    std::thread::sleep(short + Duration::from_millis(20));
    assert!(matches!(
        store.acquire_lease(chat, LeaseKind::Turn, &me).unwrap(),
        Acquire::Granted(_)
    ));
    assert_eq!(
        store.renew_lease(&mut lease).unwrap_err().code().code,
        "store.lease_lost"
    );
}
