//! Encryption at rest (spec: session-storage, Encryption at rest option):
//! a keychain-encrypted store on an in-memory keychain.

use std::sync::Arc;

use pmt_config::secrets::{Keychain, MemoryKeychain};
use pmt_store::{NewMessage, Role, Store, Visibility};

use crate::support::{TestHome, session};

const SECRET_TEXT: &str = "the launch codes are 0000";

fn keychain() -> Arc<MemoryKeychain> {
    Arc::new(MemoryKeychain::default())
}

fn encrypted(home: &TestHome, keychain: &Arc<MemoryKeychain>) -> Store {
    let scope = home.scope_with("default", &["storage.encryption=\"keychain\""]);
    Store::open_with_keychain(&scope, &(Arc::clone(keychain) as Arc<dyn Keychain>)).unwrap()
}

fn files_mention(dir: &std::path::Path, needle: &str) -> bool {
    walk(dir).into_iter().any(|path| {
        std::fs::read(&path)
            .is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle.as_bytes()))
    })
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[test]
fn an_encrypted_store_is_unreadable_as_sqlite_and_keeps_its_rows() {
    // Scenario: Store unreadable without key.
    let home = TestHome::new();
    let keys = keychain();
    let store = encrypted(&home, &keys);
    let id = session(&store).id;
    store
        .append_message(id, &NewMessage::new(Role::User, SECRET_TEXT))
        .unwrap();
    // Spilled to the blob store.
    let big = format!("{SECRET_TEXT} {}", "y".repeat(80_000));
    store
        .append_message(id, &NewMessage::new(Role::Assistant, &big))
        .unwrap();
    drop(store);
    let database = home.database("default");
    let generic = rusqlite::Connection::open(&database).unwrap();
    let read = generic.query_row("SELECT count(*) FROM sessions", [], |r| r.get::<_, i64>(0));
    assert!(read.is_err(), "a generic SQLite open reads nothing");
    let profile = database.parent().unwrap();
    assert!(
        !files_mention(profile, "launch codes"),
        "no plaintext on disk"
    );

    let store = encrypted(&home, &keys);
    let messages = store.messages(id, Visibility::Active).unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].content.as_deref(), Some(SECRET_TEXT));
    assert_eq!(messages[1].content.as_deref(), Some(big.as_str()));
    let found = store
        .search("launch", &pmt_store::SearchOptions::default())
        .unwrap();
    assert_eq!(found.hits.len(), 2);
}

#[test]
fn a_missing_key_fails_closed_without_creating_a_store() {
    // Scenario: Missing key fails closed.
    let home = TestHome::new();
    let keys = keychain();
    let store = encrypted(&home, &keys);
    session(&store);
    drop(store);
    let database = home.database("default");
    let before = std::fs::read(&database).unwrap();
    for account in keys.accounts() {
        keys.delete(&account).unwrap();
    }
    let scope = home.scope_with("default", &["storage.encryption=\"keychain\""]);
    let err =
        Store::open_with_keychain(&scope, &(Arc::clone(&keys) as Arc<dyn Keychain>)).unwrap_err();
    assert_eq!(err.code().code, "store.key_unavailable");
    assert_eq!(err.exit_code(), 78);
    assert_eq!(
        err.message(),
        "state.db is encrypted and its key is unavailable"
    );
    assert_eq!(
        std::fs::read(&database).unwrap(),
        before,
        "the store is untouched"
    );
    // Nor is a plaintext open of an encrypted file taken for a new store.
    let plain = home.scope("default");
    let err = Store::open_with_keychain(&plain, &(keychain() as Arc<dyn Keychain>)).unwrap_err();
    assert_eq!(err.exit_code(), 78);
}

#[test]
fn a_plaintext_store_under_an_encryption_mode_is_refused() {
    let home = TestHome::new();
    session(&home.store("default"));
    let scope = home.scope_with("default", &["storage.encryption=\"keychain\""]);
    let err = Store::open_with_keychain(&scope, &(keychain() as Arc<dyn Keychain>)).unwrap_err();
    assert_eq!(err.code().code, "store.encryption_mismatch");
}
