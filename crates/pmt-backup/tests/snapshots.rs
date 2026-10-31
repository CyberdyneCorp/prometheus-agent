//! Per-profile state snapshot contracts (spec: configuration-and-profiles,
//! State snapshots).

use pmt_backup::snapshot::{self, Reason};
use pmt_config::{Home, ProfileDir, ProfileName};
use rusqlite::Connection;

fn profile() -> (tempfile::TempDir, ProfileDir) {
    let temp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let home = Home::open(&temp.path().join("home")).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    (temp, profile)
}

fn rows(db: &Connection) -> Vec<String> {
    let mut stmt = db.prepare("SELECT v FROM t ORDER BY v").unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn pre_update_snapshot_restores() {
    let (_t, profile) = profile();
    profile
        .config_file()
        .set("agent.max_iterations", "11")
        .unwrap();
    let db = Connection::open(profile.path().join("state.db")).unwrap();
    db.execute_batch(
        "PRAGMA journal_mode=WAL; CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('before');",
    )
    .unwrap();
    let taken = snapshot::create(&profile, Reason::PreUpdate, Some("0.1.0 -> 0.2.0")).unwrap();
    assert!(taken.id.starts_with("snap_"));
    let config_then = std::fs::read(profile.config_file().path()).unwrap();

    profile
        .config_file()
        .set("agent.max_iterations", "12")
        .unwrap();
    db.execute("INSERT INTO t VALUES ('after')", []).unwrap();
    let restored = snapshot::restore(&profile, &taken.id).unwrap();

    assert_eq!(
        std::fs::read(profile.config_file().path()).unwrap(),
        config_then
    );
    assert_eq!(rows(&db), ["before"]);
    let listed = snapshot::list(&profile).unwrap();
    assert_eq!(listed[0].id, restored.pre_restore.id);
    assert_eq!(listed[0].reason, Reason::PreRestore);
    assert_eq!(listed[1].reason, Reason::PreUpdate);
    // The pre-restore snapshot holds the replaced state.
    let undo = snapshot::restore(&profile, &restored.pre_restore.id).unwrap();
    assert_eq!(rows(&db), ["after", "before"]);
    assert_eq!(undo.restored.reason, Reason::PreRestore);
}

#[test]
fn snapshot_holds_only_critical_state() {
    let (_t, profile) = profile();
    std::fs::write(profile.path().join("skills/big.md"), "not critical").unwrap();
    std::fs::create_dir_all(profile.path().join("cron")).unwrap();
    std::fs::write(profile.path().join("cron/jobs.toml"), "[jobs]").unwrap();
    let taken = snapshot::create(&profile, Reason::Manual, None).unwrap();
    let paths: Vec<_> = taken.members.iter().map(|m| m.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "config.toml",
            "cron/jobs.toml",
            "grants.toml",
            "secrets.toml"
        ]
    );
}

#[test]
fn prune_keeps_the_newest_manual_snapshots_only() {
    let (_t, profile) = profile();
    let update = snapshot::create(&profile, Reason::PreUpdate, None).unwrap();
    let manual: Vec<_> = (0..4)
        .map(|_| snapshot::create(&profile, Reason::Manual, None).unwrap().id)
        .collect();
    let removed = snapshot::prune(&profile, 2).unwrap();
    assert_eq!(removed, [manual[1].clone(), manual[0].clone()]);
    let left: Vec<_> = snapshot::list(&profile)
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(left, [manual[3].clone(), manual[2].clone(), update.id]);
}

#[test]
fn unknown_or_tampered_snapshots_are_refused() {
    let (_t, profile) = profile();
    let err = snapshot::restore(&profile, "snap_01J0000000000000000000000").unwrap_err();
    assert_eq!(err.exit_code(), 66);
    assert_eq!(
        snapshot::restore(&profile, "../x").unwrap_err().exit_code(),
        66
    );
    let taken = snapshot::create(&profile, Reason::Manual, None).unwrap();
    let file = profile
        .path()
        .join("snapshots")
        .join(&taken.id)
        .join("files/config.toml");
    std::fs::write(file, "tampered").unwrap();
    profile
        .config_file()
        .set("agent.max_iterations", "4")
        .unwrap();
    let before = std::fs::read(profile.config_file().path()).unwrap();
    assert_eq!(
        snapshot::restore(&profile, &taken.id)
            .unwrap_err()
            .exit_code(),
        65
    );
    assert_eq!(std::fs::read(profile.config_file().path()).unwrap(), before);
}
