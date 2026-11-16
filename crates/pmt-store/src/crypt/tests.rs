//! The encrypting VFS against real SQLite files: WAL and rollback
//! journals, reopening, other keys, plain SQLite and conversion.

use rusqlite::{Connection, OpenFlags};

use super::*;

fn dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn crypt() -> Crypt {
    Crypt::new(Key::generate().unwrap()).unwrap()
}

fn fill(conn: &Connection, rows: usize) {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, body TEXT)")
        .unwrap();
    for n in 0..rows {
        conn.execute(
            "INSERT INTO t (body) VALUES (?1)",
            [format!("secret transcript line {n} {}", "x".repeat(300))],
        )
        .unwrap();
    }
}

fn count(conn: &Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .unwrap()
}

fn never_plaintext(path: &Path) {
    for file in [path.to_path_buf(), path.with_extension("db-wal")] {
        if let Ok(bytes) = std::fs::read(&file) {
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains("secret transcript"), "{}", file.display());
        }
    }
}

#[test]
fn a_wal_store_round_trips_and_is_not_sqlite_without_the_key() {
    let tmp = dir();
    let path = tmp.path().join("state.db");
    let c = crypt();
    {
        let conn = open(&path, Some(&c)).unwrap();
        conn.pragma_update(None, "journal_mode", "wal").unwrap();
        conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        fill(&conn, 400);
        // The rows sit in the WAL until a checkpoint: read them back from a
        // second connection first (WAL frames through the VFS).
        let second = connect(&path, OpenFlags::SQLITE_OPEN_READ_ONLY, Some(&c)).unwrap();
        assert_eq!(count(&second), 400);
        never_plaintext(&path);
    }
    assert_eq!(is_plaintext(&path), Some(false));
    let conn = open(&path, Some(&c)).unwrap();
    assert_eq!(count(&conn), 400);
    // Sorting more than fits a page cache spills nowhere but memory.
    let longest: i64 = conn
        .query_row(
            "SELECT max(length(body)) FROM (SELECT body FROM t ORDER BY body DESC)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(longest > 300);
    drop(conn);
    never_plaintext(&path);
    // A generic SQLite open, and another key, cannot read it.
    let plain = Connection::open(&path).unwrap();
    assert!(
        plain
            .query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))
            .is_err()
    );
    let other = open(&path, Some(&crypt()))
        .and_then(|c| c.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0)));
    assert!(other.is_err(), "another key reads nothing");
}

#[test]
fn a_rollback_journal_store_survives_a_rolled_back_transaction() {
    let tmp = dir();
    let path = tmp.path().join("state.db");
    let c = crypt();
    let mut conn = open(&path, Some(&c)).unwrap();
    conn.pragma_update(None, "journal_mode", "delete").unwrap();
    fill(&conn, 50);
    let tx = conn.transaction().unwrap();
    tx.execute("DELETE FROM t", []).unwrap();
    tx.rollback().unwrap();
    assert_eq!(count(&conn), 50);
    drop(conn);
    never_plaintext(&path);
    assert_eq!(count(&open(&path, Some(&c)).unwrap()), 50);
}

#[test]
fn converting_writes_an_encrypted_copy_and_back() {
    let tmp = dir();
    let plain_path = tmp.path().join("plain.db");
    let conn = open(&plain_path, None).unwrap();
    fill(&conn, 30);
    let c = crypt();
    let sealed = tmp.path().join("sealed.db");
    convert(&conn, &sealed, Some(&c)).unwrap();
    assert_eq!(is_plaintext(&sealed), Some(false));
    let reopened = open(&sealed, Some(&c)).unwrap();
    assert_eq!(count(&reopened), 30);
    let back = tmp.path().join("back.db");
    convert(&reopened, &back, None).unwrap();
    assert_eq!(is_plaintext(&back), Some(true));
    assert_eq!(count(&Connection::open(&back).unwrap()), 30);
}

#[test]
fn blobs_seal_whole_and_open_with_their_key_only() {
    let key = Key::generate().unwrap();
    let sealed = seal_blob(&key, b"attachment").unwrap();
    assert!(!sealed.windows(10).any(|w| w == b"attachment"));
    assert_eq!(open_blob(&key, &sealed).unwrap(), b"attachment");
    assert!(open_blob(&Key::generate().unwrap(), &sealed).is_err());
}
