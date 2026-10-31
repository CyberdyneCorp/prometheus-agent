//! `prometheus backup` and `prometheus snapshots` against the built binary
//! (spec: configuration-and-profiles, Whole-home backup, Home restore,
//! State snapshots) and the `/snapshot` handler.

mod common;

use common::{ok, run, stderr, stdout, temp_home};
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use prometheus::slash_handlers::snapshot;

#[test]
fn backup_then_restore_needs_force_or_confirmation() {
    let home = temp_home();
    ok(&run(
        home.path(),
        &["config", "set", "agent.max_iterations", "5"],
    ));
    let archive = ok(&run(home.path(), &["backup", "create", "--label", "first"]));
    let archive = archive.trim();
    assert!(archive.ends_with("-first.tar.zst"), "{archive}");
    assert!(archive.contains("/backups/prometheus-"));
    ok(&run(
        home.path(),
        &["config", "set", "agent.max_iterations", "6"],
    ));
    // Non-interactive and the home holds other data: exit 4, nothing changed.
    let out = run(home.path(), &["backup", "restore", archive]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    let get = || {
        ok(&run(
            home.path(),
            &["config", "get", "agent.max_iterations"],
        ))
    };
    assert_eq!(get().trim(), "6");
    let out = run(
        home.path(),
        &["--json", "backup", "restore", archive, "--force"],
    );
    let doc: serde_json::Value = serde_json::from_str(&ok(&out)).unwrap();
    assert_eq!(doc["schema"], "prometheus.restore.v1");
    assert_eq!(get().trim(), "5");
    let listed = ok(&run(home.path(), &["snapshots", "list"]));
    assert!(listed.contains("pre-restore"), "{listed}");
}

#[test]
fn corrupt_archive_exits_65() {
    let home = temp_home();
    let archive = home.path().join("b.tar.zst");
    std::fs::write(&archive, b"not an archive").unwrap();
    let out = run(home.path(), &["--json", "backup", "restore"])
        .status
        .code();
    assert_eq!(out, Some(2), "missing argument is a usage error");
    let out = run(
        home.path(),
        &["backup", "restore", archive.to_str().unwrap(), "--force"],
    );
    assert_eq!(out.status.code(), Some(65), "{}", stderr(&out));
    assert!(stdout(&out).is_empty());
}

#[test]
fn snapshots_cli_create_list_restore_prune() {
    let home = temp_home();
    ok(&run(home.path(), &["config", "set", "snapshots.keep", "2"]));
    let id = ok(&run(
        home.path(),
        &["snapshots", "create", "--label", "before"],
    ));
    let id = id.trim().to_owned();
    ok(&run(
        home.path(),
        &["config", "set", "agent.max_iterations", "9"],
    ));
    ok(&run(home.path(), &["snapshots", "restore", &id]));
    let get = ok(&run(
        home.path(),
        &["config", "get", "agent.max_iterations"],
    ));
    assert_eq!(get.trim(), "500");
    let json = ok(&run(home.path(), &["--json", "snapshots", "list"]));
    let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(doc["schema"], "prometheus.snapshots.v1");
    let reasons: Vec<_> = doc["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["reason"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(reasons, ["pre-restore", "manual"]);
    ok(&run(home.path(), &["snapshots", "prune", "--keep", "0"]));
    assert!(ok(&run(home.path(), &["snapshots", "list"])).is_empty());
    let out = run(home.path(), &["snapshots", "restore", &id]);
    assert_eq!(out.status.code(), Some(66));
}

#[test]
fn slash_snapshot_command() {
    let home = temp_home();
    let profile = Home::open(home.path())
        .unwrap()
        .profile(&ProfileName::default_profile())
        .unwrap();
    let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
    assert_eq!(snapshot(&scope, "").unwrap(), "no snapshots");
    let created = snapshot(&scope, "create before the change").unwrap();
    let id = created.strip_prefix("created ").unwrap();
    assert!(snapshot(&scope, "list").unwrap().contains("manual"));
    assert!(
        snapshot(&scope, "list")
            .unwrap()
            .contains("before the change")
    );
    assert!(
        snapshot(&scope, &format!("restore {id}"))
            .unwrap()
            .starts_with("restored")
    );
    assert_eq!(
        snapshot(&scope, "prune 0").unwrap(),
        "removed 2 snapshot(s)"
    );
    let err = snapshot(&scope, "prune many").unwrap_err();
    assert_eq!(err.exit_code(), 2);
    assert!(
        err.message()
            .contains("/snapshot [create [label]|list|restore <id>|prune [n]]")
    );
}
