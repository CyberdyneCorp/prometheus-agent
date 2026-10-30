//! Integrity check and quarantine, out-of-band replacement and write
//! contention (spec: session-storage, Integrity check and quarantine;
//! Out-of-band file replacement detection; Write contention policy).

use std::io::{Seek, SeekFrom, Write};
use std::time::{Duration, Instant};

use pmt_core::doctor::Status;
use pmt_store::{NewMessage, Role, SearchOptions, Store, TitleSource, Visibility};

use crate::support::{TestHome, raw, session};

/// A store with `sessions` sessions of 20 messages each, closed and
/// checkpointed so every row is in the main file.
fn filled(home: &TestHome, sessions: usize) {
    let store = home.store("default");
    for _ in 0..sessions {
        let chat = session(&store);
        let batch: Vec<NewMessage> = (0..20)
            .map(|n| NewMessage::new(Role::User, format!("message {n} {}", "padding ".repeat(40))))
            .collect();
        store.append_messages(chat.id, &batch).unwrap();
    }
    drop(store);
    raw(&home.database("default"))
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
}

/// The page holding a message row: the page where its content hash (a
/// value stored in no other table or index) sits in the file.
fn message_page(home: &TestHome) -> u64 {
    let conn = raw(&home.database("default"));
    let hash: String = conn
        .query_row(
            "SELECT content_hash FROM messages ORDER BY seq LIMIT 1 OFFSET 7",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let size = conn
        .query_row("PRAGMA page_size", [], |r| r.get::<_, i64>(0))
        .unwrap();
    let bytes = std::fs::read(home.database("default")).unwrap();
    let at = bytes
        .windows(hash.len())
        .position(|w| w == hash.as_bytes())
        .unwrap();
    u64::try_from(at).unwrap() / u64::try_from(size).unwrap() + 1
}

/// Overwrites page `page` (1-based) with garbage.
fn damage_page(home: &TestHome, page: u64) {
    let size = raw(&home.database("default"))
        .query_row("PRAGMA page_size", [], |r| r.get::<_, i64>(0))
        .map(|n| u64::try_from(n).unwrap())
        .unwrap();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(home.database("default"))
        .unwrap();
    file.seek(SeekFrom::Start((page - 1) * size)).unwrap();
    file.write_all(&vec![0xA5; usize::try_from(size).unwrap()])
        .unwrap();
}

#[test]
fn corrupt_file_is_quarantined_and_salvaged() {
    let original = TestHome::new();
    filled(&original, 30);
    let home = original.copied();
    damage_page(&home, message_page(&home));

    let store = home.store("default");

    let report = store
        .quarantine()
        .expect("the corrupt file was quarantined")
        .clone();
    assert!(
        report
            .quarantined
            .starts_with(home.scope("default").path("quarantine"))
    );
    assert!(report.quarantined.exists(), "the original file is kept");
    assert!(report.reason.contains("quick_check"), "{}", report.reason);
    assert!(report.lost_rows() >= 1, "{report:?}");
    assert!(report.salvaged_rows() >= 30 * 20 / 2, "{report:?}");
    // The fresh store holds every salvaged row and passes its own check.
    let check: String = raw(&home.database("default"))
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(check, "ok");
    let salvaged_messages = report.salvaged["messages"];
    let stored = raw(&home.database("default"))
        .query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))
        .map(|n| u64::try_from(n).unwrap())
        .unwrap();
    assert_eq!(stored, salvaged_messages);
    let hits = store
        .search(
            "padding",
            &SearchOptions {
                limit: 1000,
                ..SearchOptions::default()
            },
        )
        .unwrap();
    assert_eq!(hits.hits.len() as u64, salvaged_messages, "indexes rebuilt");
    // Doctor reports the quarantine path and the lost rows.
    let checks = store.doctor_checks().unwrap();
    let quarantine = checks.iter().find(|c| c.name == "quarantine").unwrap();
    assert_eq!(quarantine.status, Status::Warn);
    assert!(
        quarantine
            .message
            .contains(&report.quarantined.display().to_string())
    );
    assert!(
        quarantine
            .message
            .contains(&format!("{} lost", report.lost_rows()))
    );
    // The store is writable.
    let chat = session(&store);
    store
        .append_message(chat.id, &NewMessage::new(Role::User, "after"))
        .unwrap();
}

#[test]
fn file_that_is_not_a_database_is_replaced_by_a_fresh_store() {
    let home = TestHome::new();
    let scope = home.scope("default");
    std::fs::write(
        scope.path("state.db"),
        b"this is not a SQLite database at all, just text",
    )
    .unwrap();

    let store = Store::open(&scope).unwrap();

    let report = store.quarantine().unwrap();
    assert_eq!(report.salvaged_rows(), 0);
    assert!(report.quarantined.exists());
    session(&store);
}

#[test]
fn healthy_store_is_not_quarantined() {
    let original = TestHome::new();
    filled(&original, 1);
    let home = original.copied();
    let store = home.store("default");
    assert!(store.quarantine().is_none());
    assert!(!home.scope("default").path("quarantine").exists());
}

// Windows cannot rename over a file SQLite holds open; there a restore can
// only happen while no store is open.
#[cfg(unix)]
#[test]
fn file_restored_from_backup_while_open_refuses_writes() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    let database = home.database("default");
    let backup = database.with_file_name("backup.db");
    raw(&database)
        .execute("VACUUM INTO ?1", [backup.to_str().unwrap()])
        .unwrap();
    let before = std::fs::read(&backup).unwrap();
    // A restore replaces the file (a new file renamed over the old one).
    std::fs::rename(&backup, &database).unwrap();

    let err = store
        .set_title(chat.id, Some("after restore"), TitleSource::User)
        .unwrap_err();

    assert_eq!(err.code().code, "store.replaced");
    assert_eq!(err.exit_code(), 75);
    assert_eq!(
        std::fs::read(&database).unwrap(),
        before,
        "restored file untouched"
    );
}

#[test]
fn another_store_copied_in_place_is_detected() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    raw(&home.database("default"))
        .execute(
            "UPDATE store_meta SET value = 'another-store' WHERE key = 'store_uuid'",
            [],
        )
        .unwrap();
    let err = store
        .end_session(chat.id, pmt_store::EndReason::UserExit)
        .unwrap_err();
    assert_eq!(err.code().code, "store.replaced");
}

/// Holds the write lock from another connection for `hold`.
fn lock_for(home: &TestHome, hold: Duration) -> std::thread::JoinHandle<()> {
    let database = home.database("default");
    let (ready, wait) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let conn = raw(&database);
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        ready.send(()).unwrap();
        std::thread::sleep(hold);
        conn.execute_batch("COMMIT").unwrap();
    });
    wait.recv().unwrap();
    holder
}

#[test]
fn activity_write_gives_up_fast_with_store_busy() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    let holder = lock_for(&home, Duration::from_secs(3));

    let started = Instant::now();
    let err = store.record_activity(chat.id).unwrap_err();
    let waited = started.elapsed();

    assert_eq!(err.code().code, "store.busy");
    assert_eq!(err.exit_code(), 75);
    assert!(err.message().contains("activity"), "{err}");
    assert!(waited >= Duration::from_millis(400), "{waited:?}");
    assert!(waited < Duration::from_millis(1500), "{waited:?}");
    holder.join().unwrap();
}

#[test]
fn metadata_write_retries_until_the_lock_is_released() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    let holder = lock_for(&home, Duration::from_millis(2500));

    let started = Instant::now();
    store
        .set_title(chat.id, Some("Waited"), TitleSource::User)
        .unwrap();

    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        store.session(chat.id).unwrap().title.as_deref(),
        Some("Waited")
    );
    holder.join().unwrap();
    assert_eq!(store.messages(chat.id, Visibility::All).unwrap().len(), 0);
}
