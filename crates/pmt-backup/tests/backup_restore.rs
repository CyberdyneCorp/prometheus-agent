//! Whole-home backup and restore contracts (spec: configuration-and-
//! profiles, Whole-home backup, Home restore).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pmt_backup::{CreateOptions, create, restore, sqlite};
use pmt_config::{Home, ProfileName};
use rusqlite::Connection;

fn home() -> (tempfile::TempDir, Home) {
    let temp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let home = Home::open(&temp.path().join("home")).unwrap();
    (temp, home)
}

fn options(keep: usize) -> CreateOptions {
    CreateOptions {
        keep,
        ..CreateOptions::default()
    }
}

/// A `state.db` in WAL mode with the session tables.
fn state_db(path: &Path) -> Connection {
    let db = Connection::open(path).unwrap();
    db.pragma_update(None, "journal_mode", "WAL").unwrap();
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (id INTEGER PRIMARY KEY, title TEXT);
         CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY, session INTEGER, body TEXT);",
    )
    .unwrap();
    db
}

/// Every member of an archive with its bytes.
fn read_archive(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let decoder = zstd::Decoder::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut archive = tar::Archive::new(decoder);
    archive
        .entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
            (path, bytes)
        })
        .collect()
}

fn write_archive(path: &Path, members: &BTreeMap<String, Vec<u8>>, last: &str) {
    let file = std::fs::File::create(path).unwrap();
    let mut tar = tar::Builder::new(zstd::Encoder::new(file, 3).unwrap());
    let mut order: Vec<String> = members.keys().filter(|k| *k != last).cloned().collect();
    order.push(last.to_owned());
    for name in order {
        let data = &members[&name];
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o600);
        // Raw name bytes: `set_path` refuses `..`, which a hostile archive has.
        header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
        header.set_cksum();
        tar.append(&header, data.as_slice()).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap();
}

/// Path -> contents of every file under `dir`.
fn tree(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut found = BTreeMap::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.insert(path.clone(), std::fs::read(&path).unwrap());
            }
        }
    }
    found
}

#[test]
fn live_database_is_captured_consistently() {
    let (_t, home) = home();
    let db_path = home.root().join("profiles/default/state.db");
    let db = state_db(&db_path);
    db.execute("INSERT INTO sessions (title) VALUES ('first')", [])
        .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (stop, db_path) = (stop.clone(), db_path.clone());
        std::thread::spawn(move || {
            let mut db = state_db(&db_path);
            let mut n = 0_u32;
            while !stop.load(Ordering::Relaxed) {
                let tx = db.transaction().unwrap();
                tx.execute("INSERT INTO sessions (title) VALUES (?1)", [n])
                    .unwrap();
                for _ in 0..20 {
                    tx.execute("INSERT INTO messages (session, body) VALUES (?1, 'x')", [n])
                        .unwrap();
                }
                tx.commit().unwrap();
                n += 1;
            }
            n
        })
    };
    while !home.root().join("profiles/default/state.db-wal").exists() {
        std::thread::yield_now();
    }
    let created = create(&home, &options(3)).unwrap();
    stop.store(true, Ordering::Relaxed);
    assert!(writer.join().unwrap() > 0);
    assert!(created.skipped.is_empty(), "{:?}", created.skipped);
    let members = read_archive(&created.path);
    assert!(
        !members
            .keys()
            .any(|m| m.ends_with("-wal") || m.ends_with("-shm"))
    );
    let extracted = home.root().join("extracted.db");
    std::fs::write(&extracted, &members["profiles/default/state.db"]).unwrap();
    assert!(sqlite::is_intact(&extracted));
    // Consistent: every captured session has all 20 of its messages.
    let db = Connection::open(&extracted).unwrap();
    let torn: i64 = db
        .query_row(
            "SELECT count(*) FROM (SELECT session, count(*) c FROM messages GROUP BY session) WHERE c != 20",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(torn, 0);
    let manifest: serde_json::Value = serde_json::from_slice(&members["manifest.json"]).unwrap();
    assert!(
        manifest["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["path"] == "profiles/default/state.db"
                && m["sha256"].as_str().unwrap().len() == 64)
    );
}

#[test]
fn exclusions_and_quick_set() {
    let (_t, home) = home();
    let root = home.root();
    for (path, body) in [
        ("daemon.toml", ""),
        ("run/gateway.lock", ""),
        ("logs/daemon.log", "log"),
        ("runtimes/node/bin/node", "bin"),
        ("profiles/default/cache/web/page.html", "cached"),
        ("profiles/default/skills/mine/SKILL.md", "skill"),
        ("profiles/default/cron/jobs.toml", "jobs"),
    ] {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    let full = read_archive(&create(&home, &options(3)).unwrap().path);
    for kept in [
        "daemon.toml",
        "profiles/default/config.toml",
        "profiles/default/skills/mine/SKILL.md",
    ] {
        assert!(full.contains_key(kept), "{kept}");
    }
    for dropped in [
        "run/gateway.lock",
        "logs/daemon.log",
        "runtimes/node/bin/node",
        "profiles/default/cache/web/page.html",
    ] {
        assert!(!full.contains_key(dropped), "{dropped}");
    }
    let quick = CreateOptions {
        quick: true,
        keep: 3,
        ..CreateOptions::default()
    };
    let quick = read_archive(&create(&home, &quick).unwrap().path);
    assert!(quick.contains_key("profiles/default/cron/jobs.toml"));
    assert!(quick.contains_key("profiles/default/secrets.toml"));
    assert!(!quick.contains_key("profiles/default/skills/mine/SKILL.md"));
}

#[test]
fn retention_keeps_the_newest() {
    let (_t, home) = home();
    let backups = home.root().join("backups");
    std::fs::create_dir_all(&backups).unwrap();
    for name in [
        "prometheus-20200101T000000Z.tar.zst",
        "prometheus-20200102T000000Z.tar.zst",
        "prometheus-20200103T000000Z-label.tar.zst",
        "notes.txt",
    ] {
        std::fs::write(backups.join(name), "old").unwrap();
    }
    let created = create(&home, &options(3)).unwrap();
    let mut left: Vec<String> = std::fs::read_dir(&backups)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    left.sort();
    let newest = created
        .path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        left,
        [
            "notes.txt".to_owned(),
            "prometheus-20200102T000000Z.tar.zst".to_owned(),
            "prometheus-20200103T000000Z-label.tar.zst".to_owned(),
            newest,
        ]
    );
    assert_eq!(created.pruned.len(), 1);
}

#[test]
fn second_concurrent_backup_is_refused() {
    let (_t, home) = home();
    let backups = home.root().join("backups");
    std::fs::create_dir_all(&backups).unwrap();
    let held = std::fs::File::create(backups.join(".backup.lock")).unwrap();
    held.lock().unwrap();
    let err = create(&home, &options(3)).unwrap_err();
    assert_eq!(err.exit_code(), 75);
    assert_eq!(err.message(), "another backup is running");
}

#[cfg(unix)]
#[test]
fn unreadable_members_are_skipped_and_reported() {
    use std::os::unix::fs::PermissionsExt;
    let (_t, home) = home();
    let secret = home.root().join("profiles/default/skills/locked.md");
    std::fs::write(&secret, "x").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::File::open(&secret).is_ok() {
        return; // running as root: nothing is unreadable
    }
    let created = create(&home, &options(3)).unwrap();
    assert_eq!(created.skipped, ["profiles/default/skills/locked.md"]);
    assert!(read_archive(&created.path).contains_key("profiles/default/config.toml"));
}

#[test]
fn corrupt_archive_writes_nothing() {
    let (temp, home) = home();
    state_db(&home.root().join("profiles/default/state.db"));
    let archive = create(&home, &options(3)).unwrap().path;
    let mut members = read_archive(&archive);
    members.insert(
        "profiles/default/config.toml".into(),
        b"schema_version = 1\n# tampered\n".to_vec(),
    );
    let tampered = temp.path().join("b.tar.zst");
    write_archive(&tampered, &members, "manifest.json");
    home.profile(&ProfileName::default_profile())
        .unwrap()
        .config_file()
        .set("agent.max_iterations", "9")
        .unwrap();
    let before = tree(home.root());
    let err = restore::plan(&home, &tampered, None).unwrap_err();
    assert_eq!(err.exit_code(), 65);
    assert!(
        err.message().contains("profiles/default/config.toml"),
        "{}",
        err.message()
    );
    assert_eq!(tree(home.root()), before);

    // A member outside the manifest, or a path escaping the home, too.
    let mut extra = read_archive(&archive);
    extra.insert("../escape".into(), b"x".to_vec());
    write_archive(&tampered, &extra, "manifest.json");
    assert_eq!(
        restore::plan(&home, &tampered, None)
            .unwrap_err()
            .exit_code(),
        65
    );
    assert_eq!(tree(home.root()), before);
    assert!(!temp.path().join("escape").exists());
}

#[test]
fn restore_round_trip_snapshots_the_replaced_state() {
    let (_t, home) = home();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    profile
        .config_file()
        .set("agent.max_iterations", "5")
        .unwrap();
    let db = state_db(&profile.path().join("state.db"));
    db.execute("INSERT INTO sessions (title) VALUES ('kept')", [])
        .unwrap();
    let archive = create(&home, &options(3)).unwrap().path;
    let backed_up = std::fs::read(profile.config_file().path()).unwrap();

    profile
        .config_file()
        .set("agent.max_iterations", "8")
        .unwrap();
    for title in ["newer-1", "newer-2"] {
        db.execute("INSERT INTO sessions (title) VALUES (?1)", [title])
            .unwrap();
    }
    let plan = restore::plan(&home, &archive, None).unwrap();
    assert!(plan.holds_data);
    assert_eq!(
        plan.warnings,
        ["restoring an older backup: 2 sessions and 0 messages newer than the backup will be lost"]
    );
    let restored = plan.apply().unwrap();
    assert_eq!(
        std::fs::read(profile.config_file().path()).unwrap(),
        backed_up
    );
    // The open connection sees the restored pages.
    let titles: i64 = db
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(titles, 1);
    assert_eq!(restored.snapshots.len(), 1);
    assert_eq!(restored.snapshots[0].reason, pmt_backup::Reason::PreRestore);
    let snapshots = pmt_backup::snapshot::list(&profile).unwrap();
    assert_eq!(snapshots[0].reason, pmt_backup::Reason::PreRestore);
}

#[test]
fn profile_restore_touches_only_that_profile() {
    let (_t, home) = home();
    let work = home.create_profile("work", None).unwrap();
    let default = home.profile(&ProfileName::default_profile()).unwrap();
    work.config_file().set("agent.max_iterations", "3").unwrap();
    let archive = create(&home, &options(3)).unwrap().path;
    work.config_file().set("agent.max_iterations", "4").unwrap();
    default
        .config_file()
        .set("agent.max_iterations", "6")
        .unwrap();
    let work_name = ProfileName::parse("work").unwrap();
    restore::plan(&home, &archive, Some(&work_name))
        .unwrap()
        .apply()
        .unwrap();
    let read =
        |p: &pmt_config::ProfileDir| std::fs::read_to_string(p.config_file().path()).unwrap();
    assert!(read(&work).contains("max_iterations = 3"));
    assert!(read(&default).contains("max_iterations = 6"));
    let missing = ProfileName::parse("ghost").unwrap();
    assert!(restore::plan(&home, &archive, Some(&missing)).is_err());
}

#[test]
fn restore_refuses_a_profile_in_use() {
    let (_t, home) = home();
    let archive = create(&home, &options(3)).unwrap().path;
    let lock = pmt_core::runtime::WriterLock::acquire(
        home.root(),
        pmt_core::runtime::Role::Chat,
        vec!["default".into()],
    )
    .unwrap();
    let err = restore::plan(&home, &archive, None).unwrap_err();
    assert_eq!(err.exit_code(), 75);
    assert!(err.message().contains("'default'"));
    drop(lock);
}
