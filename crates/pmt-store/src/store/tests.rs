//! Open-path behaviour that needs synthetic migrations or the private
//! checkpoint and pool state.

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use rusqlite::Connection;

use super::{CHECKPOINT_EVERY, MAX_READERS, Store};
use crate::migrate::{MIGRATIONS, Migration};
use crate::{NewSession, Source};

fn scope(root: &std::path::Path) -> ProfileScope {
    let home = Home::open(root).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap()
}

/// The real v1 plus four trivial steps.
fn five_versions() -> Vec<Migration> {
    const EXTRA: [&str; 4] = [
        "CREATE TABLE t2 (x INTEGER) STRICT;",
        "CREATE TABLE t3 (x INTEGER) STRICT;",
        "CREATE TABLE t4 (x INTEGER) STRICT;",
        "CREATE TABLE t5 (x INTEGER) STRICT;",
    ];
    let mut all = MIGRATIONS.to_vec();
    for (version, sql) in (2..).zip(EXTRA) {
        all.push(Migration {
            version,
            min_read_version: 1,
            sql,
        });
    }
    all
}

fn meta(path: &std::path::Path, key: &str) -> String {
    Connection::open(path)
        .unwrap()
        .query_row("SELECT value FROM store_meta WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .unwrap()
}

fn backups(scope: &ProfileScope) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(scope.path("backups/state"))
        .map(|dir| {
            dir.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[test]
fn older_store_is_upgraded_with_a_backup() {
    let root = tempfile::tempdir().unwrap();
    let scope = scope(root.path());
    let all = five_versions();
    let store = Store::open_with(&scope, &all[..3]).unwrap();
    let session = store.create_session(&NewSession::new(Source::Cli)).unwrap();
    drop(store);
    let db = scope.path("state.db");
    assert_eq!(meta(&db, "schema_version"), "3");
    assert!(backups(&scope).is_empty(), "a fresh store needs no backup");

    let store = Store::open_with(&scope, &all).unwrap();
    assert_eq!(meta(&db, "schema_version"), "5");
    let names = backups(&scope);
    assert_eq!(names.len(), 1, "{names:?}");
    assert!(names[0].starts_with("state-v3-"), "{names:?}");
    let backup = Connection::open(scope.path("backups/state").join(&names[0])).unwrap();
    let kept: i64 = backup
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(kept, 1, "the backup holds the pre-migration data");
    for table in ["t4", "t5"] {
        let exists: bool = Connection::open(&db)
            .unwrap()
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = ?1)",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        assert!(exists, "{table}");
    }
    assert_eq!(store.session(session.id).unwrap().id, session.id);
}

#[test]
fn only_the_newest_three_backups_are_kept() {
    let root = tempfile::tempdir().unwrap();
    let scope = scope(root.path());
    let all = five_versions();
    for end in 1..=5 {
        drop(Store::open_with(&scope, &all[..end]).unwrap());
    }
    let names = backups(&scope);
    let versions: Vec<&str> = names.iter().map(|n| &n[..8]).collect();
    assert_eq!(versions, ["state-v2", "state-v3", "state-v4"], "{names:?}");
}

#[test]
fn passive_checkpoint_runs_every_thousand_writes() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&scope(root.path())).unwrap();
    let session = store.create_session(&NewSession::new(Source::Cli)).unwrap();
    assert_eq!(store.writes_since_checkpoint(), 1);
    for _ in 1..CHECKPOINT_EVERY - 1 {
        store.reopen_session(session.id).unwrap();
    }
    assert_eq!(store.writes_since_checkpoint(), CHECKPOINT_EVERY - 1);
    store.reopen_session(session.id).unwrap();
    assert_eq!(store.writes_since_checkpoint(), 0, "checkpointed");
}

#[test]
fn reader_pool_never_exceeds_its_bound() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&scope(root.path())).unwrap();
    let session = store.create_session(&NewSession::new(Source::Cli)).unwrap();
    std::thread::scope(|threads| {
        for _ in 0..3 * MAX_READERS {
            threads.spawn(|| {
                for _ in 0..20 {
                    store.session(session.id).unwrap();
                }
            });
        }
    });
    let open = store.readers.open_connections();
    assert!((1..=MAX_READERS).contains(&open), "{open}");
}
