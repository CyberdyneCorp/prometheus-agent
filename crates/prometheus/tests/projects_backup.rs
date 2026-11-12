//! Projects survive whole-home backups, quick snapshots and profile
//! export/import (spec: projects, Projects in backups and exports).

mod common;

use std::path::{Path, PathBuf};

use common::{ok, run, stderr, temp_home};
use serde_json::Value;

fn dir(root: &Path, rel: &str) -> PathBuf {
    let path = root.join(rel);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::canonicalize(path).unwrap()
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn show(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    let doc: Value = serde_json::from_str(&ok(&run(home, &all))).unwrap();
    doc["project"].clone()
}

/// The 56.2 verification: a backup round trip into another home.
#[test]
fn a_whole_home_backup_restores_projects_and_the_active_one() {
    let home = temp_home();
    let api = dir(home.path(), "src/api");
    ok(&run(
        home.path(),
        &["projects", "create", "api", s(&api), "--use"],
    ));
    ok(&run(home.path(), &["projects", "create", "old"]));
    ok(&run(home.path(), &["projects", "archive", "old"]));
    let archive = ok(&run(home.path(), &["backup", "create"]));
    let other = temp_home();
    let out = run(
        other.path(),
        &["backup", "restore", archive.trim(), "--force"],
    );
    ok(&out);
    let project = show(other.path(), &["projects", "show", "api"]);
    assert_eq!(project["active"], true);
    assert_eq!(project["primary_path"], s(&api));
    let old = show(other.path(), &["projects", "show", "old"]);
    assert_eq!(old["archived"], true);
}

#[test]
fn a_quick_snapshot_restores_projects() {
    let home = temp_home();
    let api = dir(home.path(), "src/api");
    ok(&run(
        home.path(),
        &["projects", "create", "api", s(&api), "--use"],
    ));
    let id = ok(&run(home.path(), &["snapshots", "create"]));
    ok(&run(home.path(), &["projects", "archive", "api"]));
    ok(&run(home.path(), &["snapshots", "restore", id.trim()]));
    let project = show(home.path(), &["projects", "show", "api"]);
    assert_eq!(project["archived"], false);
    assert_eq!(project["active"], true);
}

/// Scenario: Missing folder flagged.
#[test]
fn an_imported_profile_keeps_folders_missing_on_this_machine() {
    let home = temp_home();
    let elsewhere = temp_home();
    let gone = dir(elsewhere.path(), "home/a/src/x");
    let kept = dir(home.path(), "src/kept");
    ok(&run(
        home.path(),
        &["projects", "create", "x", s(&gone), s(&kept), "--use"],
    ));
    let archive = home.path().join("default.tar.zst");
    ok(&run(
        home.path(),
        &["profile", "export", "default", "-o", s(&archive)],
    ));
    // The other machine has no such path.
    drop(elsewhere);
    assert!(!gone.exists());
    let out = run(
        home.path(),
        &["profile", "import", s(&archive), "--as", "copy"],
    );
    ok(&out);
    let project = show(home.path(), &["-p", "copy", "projects", "show", "x"]);
    assert_eq!(project["active"], true);
    let folders = project["folders"].as_array().unwrap();
    assert_eq!(folders.len(), 2, "{project}");
    let flag = |path: &Path| {
        folders
            .iter()
            .find(|f| f["path"] == s(path))
            .unwrap_or_else(|| panic!("{} not listed: {project}", path.display()))["missing"]
            .clone()
    };
    assert_eq!(flag(&gone), true);
    assert_eq!(flag(&kept), false);
    let text = ok(&run(home.path(), &["-p", "copy", "projects", "show", "x"]));
    let line = text
        .lines()
        .find(|l| l.contains(s(&gone)))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(line.ends_with("missing"), "{text}");
    assert!(
        stderr(&out).contains("imported profile"),
        "{}",
        stderr(&out)
    );
}
