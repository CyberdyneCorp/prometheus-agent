//! `prometheus profile` against the built binary (spec: configuration-and-
//! profiles, Profile name rules, Profiles are islands, Profile lifecycle
//! commands, Safe profile deletion, Profile selection at process start).

mod common;

use common::{ok, run, run_with_stdin, stderr, temp_home};
use pmt_core::runtime::{Role, WriterLock};

#[test]
fn invalid_name_exits_2_and_creates_nothing() {
    let home = temp_home();
    let out = run(home.path(), &["profile", "create", "../evil"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("invalid profile name"));
    let out = run(home.path(), &["profile", "create", "root"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("profile name 'root' is reserved"));
    let names: Vec<_> = std::fs::read_dir(home.path().join("profiles"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["default"]);
    assert!(!home.path().join("evil").exists());
}

#[test]
fn profiles_share_no_state() {
    let home = temp_home();
    ok(&run(home.path(), &["profile", "create", "work"]));
    ok(&run(
        home.path(),
        &["-p", "work", "config", "set", "agent.max_iterations", "7"],
    ));
    ok(&run_with_stdin(
        home.path(),
        &["-p", "work", "secrets", "set", "WORK_TOKEN"],
        "work-secret-1",
    ));
    std::fs::write(home.path().join("profiles/work/skills/only-work.md"), "x").unwrap();
    let get = |profile: &str| {
        ok(&run(
            home.path(),
            &["-p", profile, "config", "get", "agent.max_iterations"],
        ))
    };
    assert_eq!(get("work").trim(), "7");
    assert_eq!(get("default").trim(), "500");
    assert!(ok(&run(home.path(), &["-p", "work", "secrets", "list"])).contains("WORK_TOKEN"));
    assert!(!ok(&run(home.path(), &["secrets", "list"])).contains("WORK_TOKEN"));
    let out = run(home.path(), &["secrets", "get", "WORK_TOKEN"]);
    assert_eq!(out.status.code(), Some(78));
    assert!(
        !home
            .path()
            .join("profiles/default/skills/only-work.md")
            .exists()
    );
}

#[test]
fn flag_beats_sticky_selection() {
    let home = temp_home();
    ok(&run(home.path(), &["profile", "create", "work"]));
    ok(&run(home.path(), &["profile", "create", "home"]));
    ok(&run(home.path(), &["profile", "use", "work"]));
    assert_eq!(
        std::fs::read_to_string(home.path().join("active_profile")).unwrap(),
        "work\n"
    );
    let path = |args: &[&str]| ok(&run(home.path(), args)).trim().to_owned();
    assert!(path(&["config", "path"]).contains("profiles/work"));
    assert!(path(&["-p", "home", "config", "path"]).contains("profiles/home"));
    let listed = ok(&run(home.path(), &["profile", "list"]));
    assert_eq!(listed, "  default\n  home\n* work\n");
    ok(&run(home.path(), &["profile", "use", "default"]));
    assert!(!home.path().join("active_profile").exists());
}

#[test]
fn clone_without_secrets() {
    let home = temp_home();
    ok(&run(
        home.path(),
        &["config", "set", "agent.max_iterations", "42"],
    ));
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "MAIN_KEY"],
        "main-secret-1",
    ));
    ok(&run(
        home.path(),
        &["profile", "create", "coder", "--clone-from", "default"],
    ));
    let read = |p: &str| std::fs::read_to_string(home.path().join(p)).unwrap();
    assert_eq!(
        read("profiles/coder/config.toml"),
        read("profiles/default/config.toml")
    );
    assert!(!read("profiles/coder/secrets.toml").contains("main-secret"));
    let out = run(home.path(), &["profile", "create", "coder"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("profile 'coder' already exists"));
}

#[test]
fn deletion_needs_confirmation_and_refuses_profiles_in_use() {
    let home = temp_home();
    ok(&run(home.path(), &["profile", "create", "old"]));
    ok(&run(home.path(), &["profile", "use", "old"]));
    // Non-interactive without --yes: exit 4, nothing removed.
    let out = run(home.path(), &["profile", "delete", "old"]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert!(home.path().join("profiles/old").is_dir());
    // A running process uses it: refused with 75.
    let lock = WriterLock::acquire(home.path(), Role::Chat, vec!["old".into()]).unwrap();
    let out = run(home.path(), &["profile", "delete", "old", "--yes"]);
    assert_eq!(out.status.code(), Some(75), "{}", stderr(&out));
    assert!(stderr(&out).contains("profile 'old' is in use"));
    assert!(home.path().join("profiles/old").is_dir());
    drop(lock);
    ok(&run(home.path(), &["profile", "delete", "old", "--yes"]));
    assert!(!home.path().join("profiles/old").exists());
    assert!(home.path().join("profiles/.deleted/old").is_file());
    assert!(
        !home.path().join("active_profile").exists(),
        "selection cleared"
    );
    // Nothing recreates it implicitly: selecting it is an unknown profile.
    let out = run(home.path(), &["-p", "old", "config", "path"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(!home.path().join("profiles/old").exists());
    let out = run(home.path(), &["profile", "delete", "default", "--yes"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn rename_and_show() {
    let home = temp_home();
    ok(&run(home.path(), &["profile", "create", "a"]));
    ok(&run(home.path(), &["profile", "rename", "a", "b"]));
    let shown = ok(&run(home.path(), &["--json", "profile", "show", "b"]));
    let doc: serde_json::Value = serde_json::from_str(&shown).unwrap();
    assert_eq!(doc["schema"], "prometheus.profile.v1");
    assert_eq!(doc["name"], "b");
    assert_eq!(doc["secrets_backend"], "file");
    assert!(!home.path().join("profiles/a").exists());
}
