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

/// The real v1 to v3 plus two trivial steps.
fn five_versions() -> Vec<Migration> {
    const EXTRA: [&str; 2] = [
        "CREATE TABLE t4 (x INTEGER) STRICT;",
        "CREATE TABLE t5 (x INTEGER) STRICT;",
    ];
    let mut all = MIGRATIONS.to_vec();
    assert_eq!(
        all.len(),
        3,
        "update the synthetic steps with new migrations"
    );
    for (version, sql) in (4..).zip(EXTRA) {
        all.push(Migration {
            version,
            min_read_version: 1,
            sql,
            rebuilds_tables: false,
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

#[test]
fn v1_store_upgrades_to_the_current_schema_with_indexes_and_relaxed_lineage() {
    let root = tempfile::tempdir().unwrap();
    let scope = scope(root.path());
    drop(Store::open_with(&scope, &MIGRATIONS[..1]).unwrap());
    let db = scope.path("state.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "INSERT INTO sessions (id, profile_id, source, origin, started_at, last_activity_at, \
           root_session_id) VALUES ('ses_01J00000000000000000000000', 'p', 'cli', '{\"kind\":\"none\"}', 1, 1, \
           'ses_01J00000000000000000000000');
         INSERT INTO sessions (id, profile_id, source, origin, started_at, last_activity_at, \
           parent_session_id, lineage_kind, root_session_id) VALUES ('ses_01J00000000000000000000001', \
           'p', 'cli', '{\"kind\":\"none\"}', 2, 2, 'ses_01J00000000000000000000000', 'fork', \
           'ses_01J00000000000000000000000');
         INSERT INTO messages (id, session_id, seq, role, content, content_hash, created_at) \
           VALUES ('msg_01J00000000000000000000000', 'ses_01J00000000000000000000000', 1, 'user', \
           'upgrade kubernetes', 'h', 1);",
    )
    .unwrap();
    drop(conn);

    let store = Store::open(&scope).unwrap();

    assert_eq!(
        meta(&db, "schema_version"),
        crate::SCHEMA_VERSION.to_string()
    );
    assert_eq!(meta(&db, "index_stale"), "0");
    let hits = store
        .search("kubernetes", &crate::SearchOptions::default())
        .unwrap();
    assert_eq!(hits.hits.len(), 1, "existing text indexed by the migration");
    let parent = "ses_01J00000000000000000000000".parse().unwrap();
    store.delete_session(parent, false).unwrap();
    let fork = store
        .session("ses_01J00000000000000000000001".parse().unwrap())
        .unwrap();
    assert!(
        fork.parent_deleted,
        "v2 allows a kept child without a parent"
    );
}
