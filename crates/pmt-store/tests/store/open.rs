//! Opening a store (spec: session-storage, Store opened only through
//! `ProfileScope`; Owner-only file permissions; Embedded versioned
//! migrations; WAL mode, bounded readers and checkpoints).

use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use pmt_store::{NewMessage, Role, Store, Visibility};

use crate::support::{TestHome, raw, session};

#[test]
fn copied_database_is_refused_under_another_profile() {
    let home = TestHome::new();
    let work = home.store("work");
    session(&work);
    drop(work);
    drop(home.store("home"));
    let work_id = home.scope("work").profile().id().unwrap();
    let home_id = home.scope("home").profile().id().unwrap();
    std::fs::copy(home.database("work"), home.database("home")).unwrap();
    let before = std::fs::read(home.database("home")).unwrap();

    let err = Store::open(&home.scope("home")).unwrap_err();

    assert_eq!(err.code().code, "store.profile_mismatch");
    assert!(err.message().contains(&work_id.to_string()), "{err}");
    assert!(err.message().contains(&home_id.to_string()), "{err}");
    assert_eq!(
        std::fs::read(home.database("home")).unwrap(),
        before,
        "file untouched"
    );
}

#[test]
fn two_profiles_in_one_process_use_separate_stores() {
    let home = TestHome::new();
    let (work, private) = (home.store("work"), home.store("home"));
    let chat = session(&private);
    private
        .append_message(chat.id, &NewMessage::new(Role::User, "hello"))
        .unwrap();

    assert_eq!(private.messages(chat.id, Visibility::All).unwrap().len(), 1);
    assert_eq!(
        work.session(chat.id).unwrap_err().code().code,
        "session.not_found"
    );
    assert!(work.messages(chat.id, Visibility::All).unwrap().is_empty());
    let count: i64 = raw(&home.database("work"))
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[cfg(unix)]
#[test]
fn store_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let home = TestHome::new();
    let store = home.store("default");
    session(&store);
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let db = home.database("default");
    for suffix in ["", "-wal", "-shm"] {
        let path = std::path::PathBuf::from(format!("{}{suffix}", db.display()));
        assert_eq!(mode(&path), 0o600, "{}", path.display());
    }
    assert_eq!(mode(store.location().blobs()), 0o700);
    let blob = store.put_blob(b"bytes").unwrap();
    let file = store
        .location()
        .blobs()
        .join(&blob.sha256()[..2])
        .join(blob.sha256());
    assert_eq!(mode(&file), 0o600);
}

#[cfg(unix)]
#[test]
fn loose_permissions_are_tightened_with_a_warning() {
    use std::os::unix::fs::PermissionsExt;
    let home = TestHome::new();
    drop(home.store("default"));
    let db = home.database("default");
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();

    let logs = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&logs);
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || Writer(Arc::clone(&sink)))
        .finish();
    let scope = home.scope("default");
    drop(tracing::subscriber::with_default(subscriber, || {
        Store::open(&scope).unwrap()
    }));

    let mode = std::fs::metadata(&db).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let text = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    let line = text
        .lines()
        .find(|l| l.contains("WARN"))
        .unwrap_or_default();
    assert!(line.contains(&db.display().to_string()), "{text}");
    assert!(line.contains("644"), "{text}");
}

struct Writer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Writer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn newer_store_opens_read_only_and_refuses_writes() {
    let home = TestHome::new();
    let store = home.store("default");
    let existing = session(&store);
    drop(store);
    let newer = (pmt_store::SCHEMA_VERSION + 1).to_string();
    raw(&home.database("default"))
        .execute(
            "UPDATE store_meta SET value = ?1 WHERE key = 'schema_version'",
            [&newer],
        )
        .unwrap();

    let store = home.store("default");

    assert!(store.is_read_only());
    assert_eq!(
        store.session(existing.id).unwrap().id,
        existing.id,
        "reads work"
    );
    let err = store
        .append_message(existing.id, &NewMessage::new(Role::User, "hi"))
        .unwrap_err();
    assert_eq!(err.code().code, "store.schema_too_new");
    assert!(err.message().contains(&format!("version {newer}")), "{err}");
    let ours = format!("version {}", pmt_store::SCHEMA_VERSION);
    assert!(err.message().contains(&ours), "{err}");
    drop(store);
    let version: String = raw(&home.database("default"))
        .query_row(
            "SELECT value FROM store_meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, newer, "never downgraded");
}

#[test]
fn store_a_newer_build_locked_out_cannot_be_opened() {
    let home = TestHome::new();
    drop(home.store("default"));
    raw(&home.database("default"))
        .execute_batch(
            "UPDATE store_meta SET value = '9' WHERE key = 'schema_version';
             UPDATE store_meta SET value = '8' WHERE key = 'min_read_version';",
        )
        .unwrap();
    let err = Store::open(&home.scope("default")).unwrap_err();
    assert_eq!(err.code().code, "store.schema_too_new");
    assert!(err.message().contains("version 8"), "{err}");
}

#[test]
fn deleted_profile_store_is_not_opened() {
    let home = TestHome::new();
    let scope = home.scope("work");
    let store = Store::open(&scope).unwrap();
    let chat = session(&store);
    std::fs::create_dir_all(home.home.root().join("profiles/.deleted")).unwrap();
    std::fs::write(home.home.root().join("profiles/.deleted/work"), "").unwrap();
    assert_eq!(
        Store::open(&scope).unwrap_err().code().code,
        "profile.deleted"
    );
    let err = store
        .append_message(chat.id, &NewMessage::new(Role::User, "late"))
        .unwrap_err();
    assert_eq!(err.code().code, "profile.deleted");
}

#[test]
fn wal_is_the_default_and_delete_mode_is_honoured() {
    let home = TestHome::new();
    drop(home.store("default"));
    let mode = |path: &std::path::Path| -> String {
        raw(path)
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(mode(&home.database("default")), "wal");
    let scope = home.scope_with("default", &["storage.journal_mode=delete"]);
    drop(Store::open(&scope).unwrap());
    assert_eq!(mode(&home.database("default")), "delete");
}

#[test]
fn readers_do_not_block_the_writer() {
    let home = TestHome::new();
    let scope = home.scope("default");
    let store = Store::open(&scope).unwrap();
    let chat = session(&store);
    let db = home.database("default");
    let readers = 8;
    let reading = Arc::new(Barrier::new(readers + 1));
    let appended = Arc::new(Barrier::new(readers + 1));
    std::thread::scope(|threads| {
        for _ in 0..readers {
            let (reading, appended, db) = (Arc::clone(&reading), Arc::clone(&appended), db.clone());
            threads.spawn(move || {
                let mut conn = raw(&db);
                let tx = conn.transaction().unwrap();
                let _: i64 = tx
                    .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
                    .unwrap();
                reading.wait();
                appended.wait();
                drop(tx);
            });
        }
        reading.wait();
        let started = Instant::now();
        store
            .append_message(chat.id, &NewMessage::new(Role::User, "while reading"))
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
        appended.wait();
    });
    assert_eq!(store.messages(chat.id, Visibility::All).unwrap().len(), 1);
}
