//! The blob store and its collector (spec: session-storage,
//! Content-addressed blob store with garbage collection).

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use pmt_store::{BlobRef, NewMessage, Role, Store};

use crate::support::{TestHome, raw, session};

fn path(store: &Store, blob: &BlobRef) -> PathBuf {
    store
        .location()
        .blobs()
        .join(&blob.sha256()[..2])
        .join(blob.sha256())
}

/// Ages a blob file past the 24 h grace period.
fn age(file: &PathBuf, hours: u64) {
    let then = SystemTime::now() - Duration::from_secs(hours * 3600);
    std::fs::File::options()
        .append(true)
        .open(file)
        .unwrap()
        .set_modified(then)
        .unwrap();
}

#[test]
fn blobs_are_content_addressed_and_deduplicated() {
    let home = TestHome::new();
    let store = home.store("default");
    let a = store.put_blob(b"abc").unwrap();
    let b = store.put_blob(b"abc").unwrap();
    assert_eq!(a, b);
    assert_eq!(
        a.to_string(),
        "blob:sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(BlobRef::parse(&a.to_string()), Some(a.clone()));
    assert!(path(&store, &a).is_file());
    assert_eq!(store.get_blob(&a).unwrap(), b"abc");
}

#[test]
fn unreferenced_old_blob_is_collected_and_referenced_one_is_not() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    let big = "r".repeat(100 * 1024);
    let message = NewMessage::new(Role::Tool, big.clone());
    store.append_message(chat.id, &message).unwrap();
    let referenced = BlobRef::of(big.as_bytes());
    let orphan = store.put_blob(b"nobody points here").unwrap();
    age(&path(&store, &referenced), 48);
    age(&path(&store, &orphan), 48);

    let report = store.collect_blobs().unwrap();

    assert_eq!(report.deleted, std::slice::from_ref(&orphan));
    assert_eq!(report.referenced, 1);
    assert!(!path(&store, &orphan).exists());
    assert!(path(&store, &referenced).exists());
    assert_eq!(store.blob_references(&referenced).unwrap(), 1);
}

#[test]
fn freshly_written_blob_is_not_collected() {
    let home = TestHome::new();
    let store = home.store("default");
    let pending = store
        .put_blob(b"its message has not committed yet")
        .unwrap();
    age(&path(&store, &pending), 0);
    let report = store.collect_blobs().unwrap();
    assert!(report.deleted.is_empty());
    assert_eq!(report.young, 1);
    assert!(path(&store, &pending).exists());
}

#[test]
fn deleting_the_last_reference_makes_a_blob_collectable() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    let big = "d".repeat(80 * 1024);
    let message = NewMessage::new(Role::Tool, big.clone());
    store.append_message(chat.id, &message).unwrap();
    let blob = BlobRef::of(big.as_bytes());
    raw(&home.database("default"))
        .execute(
            "DELETE FROM messages WHERE id = ?1",
            [message.id.to_string()],
        )
        .unwrap();
    assert_eq!(store.blob_references(&blob).unwrap(), 0);
    age(&path(&store, &blob), 25);
    assert_eq!(store.collect_blobs().unwrap().deleted, [blob]);
}

#[test]
fn explicit_owner_references_keep_blobs() {
    let home = TestHome::new();
    let store = home.store("default");
    let blob = store.put_blob(b"trajectory").unwrap();
    store.reference_blob(&blob, "trajectory", "ses_X").unwrap();
    store.reference_blob(&blob, "attachment", "att_1").unwrap();
    assert_eq!(store.blob_references(&blob).unwrap(), 2);
    age(&path(&store, &blob), 30);
    assert!(store.collect_blobs().unwrap().deleted.is_empty());
    store.release_blob(&blob, "trajectory", "ses_X").unwrap();
    store.release_blob(&blob, "attachment", "att_1").unwrap();
    assert_eq!(store.collect_blobs().unwrap().deleted, [blob]);
}

#[test]
fn corrupt_and_missing_blobs_are_reported() {
    let home = TestHome::new();
    let store = home.store("default");
    let blob = store.put_blob(b"original").unwrap();
    std::fs::write(path(&store, &blob), b"tampered").unwrap();
    assert_eq!(
        store.get_blob(&blob).unwrap_err().code().code,
        "store.blob_corrupt"
    );
    std::fs::remove_file(path(&store, &blob)).unwrap();
    assert_eq!(
        store.get_blob(&blob).unwrap_err().code().code,
        "store.blob_missing"
    );
}

#[test]
fn abandoned_temp_files_are_cleaned_after_the_grace_period() {
    let home = TestHome::new();
    let store = home.store("default");
    let dir = store.location().blobs().join("ab");
    std::fs::create_dir_all(&dir).unwrap();
    let (old, fresh) = (dir.join(".abc.tmp.1.0"), dir.join(".abd.tmp.1.1"));
    std::fs::write(&old, b"partial").unwrap();
    std::fs::write(&fresh, b"partial").unwrap();
    age(&old, 30);
    let report = store.collect_blobs().unwrap();
    assert_eq!(report.temp_files, 1);
    assert!(!old.exists());
    assert!(fresh.exists());
}
