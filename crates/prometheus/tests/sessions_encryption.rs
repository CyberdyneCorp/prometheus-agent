//! Encryption at rest through the binary (spec: session-storage,
//! Encryption at rest option), with a passphrase-derived key: the offline
//! conversion, a store a generic SQLite tool cannot read, and a missing
//! key failing closed. The keychain variant of the same scenarios runs on
//! an in-memory keychain in `crates/pmt-store/tests/store/encryption.rs`.

mod common;

use std::path::Path;

use common::{mock_home, ok, run, run_with_stdin, stderr};

fn database(home: &Path) -> std::path::PathBuf {
    home.join("profiles/default/state.db")
}

fn secret_on_disk(home: &Path) -> bool {
    common::files_under(&home.join("profiles/default"))
        .into_iter()
        .filter(|p| {
            let name = p.to_string_lossy();
            name.contains("state.db") || name.contains("/blobs/")
        })
        .any(|p| std::fs::read(&p).is_ok_and(|b| b.windows(13).any(|w| w == b"launch codes ")))
}

#[test]
fn an_encrypted_store_is_unreadable_and_a_missing_key_fails_closed() {
    // Scenarios: Store unreadable without key; Missing key fails closed.
    let home = mock_home();
    ok(&run(
        home.path(),
        &["chat", "-z", "the launch codes are 0000"],
    ));
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "STATE_DB_PASSPHRASE"],
        "correct horse battery staple\n",
    ));
    let out = ok(&run(
        home.path(),
        &["sessions", "encrypt", "--mode", "passphrase"],
    ));
    let _ = out;
    let config = std::fs::read_to_string(home.path().join("profiles/default/config.toml")).unwrap();
    assert!(config.contains("encryption = \"passphrase\""), "{config}");
    assert!(
        !secret_on_disk(home.path()),
        "no plaintext transcript on disk"
    );
    let generic = rusqlite::Connection::open(database(home.path())).unwrap();
    let read = generic.query_row("SELECT count(*) FROM sessions", [], |r| r.get::<_, i64>(0));
    assert!(read.is_err(), "a generic SQLite open reads nothing");
    drop(generic);

    // The agent keeps working on the encrypted store.
    ok(&run(home.path(), &["chat", "-z", "more launch codes 1111"]));
    let found = ok(&run(
        home.path(),
        &["--json", "sessions", "search", "launch"],
    ));
    assert!(found.contains("0000") && found.contains("1111"), "{found}");
    assert!(!secret_on_disk(home.path()));

    // Without the passphrase: exit 78, and no new empty store.
    let before = std::fs::read(database(home.path())).unwrap();
    ok(&run(
        home.path(),
        &["secrets", "unset", "STATE_DB_PASSPHRASE"],
    ));
    let out = run(home.path(), &["chat", "-z", "hello"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("state.db is encrypted and its key is unavailable"),
        "{}",
        stderr(&out)
    );
    assert_eq!(std::fs::read(database(home.path())).unwrap(), before);

    // Back to plaintext.
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "STATE_DB_PASSPHRASE"],
        "correct horse battery staple\n",
    ));
    ok(&run(home.path(), &["sessions", "decrypt"]));
    let generic = rusqlite::Connection::open(database(home.path())).unwrap();
    let sessions: i64 = generic
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert!(sessions >= 2);
}

#[test]
fn a_plaintext_store_under_an_encryption_mode_is_refused() {
    let home = mock_home();
    ok(&run(home.path(), &["chat", "-z", "hi"]));
    common::append_config(home.path(), "\n[storage]\nencryption = \"passphrase\"\n");
    let out = run(home.path(), &["sessions", "list"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("sessions encrypt"),
        "{}",
        stderr(&out)
    );
}
