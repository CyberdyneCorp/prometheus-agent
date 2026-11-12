//! `prometheus projects` against the built binary (spec: projects, Project
//! records; Project commands; Active project and path ownership; Archive
//! and restore).

mod common;

use std::path::{Path, PathBuf};

use common::{command, ok, run, stderr, temp_home};
use serde_json::Value;

/// A new directory `rel` under the test home, canonical.
fn dir(home: &Path, rel: &str) -> PathBuf {
    let path = home.join(rel);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::canonicalize(path).unwrap()
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn show(home: &Path, project: &str) -> Value {
    let out = ok(&run(home, &["--json", "projects", "show", project]));
    let doc: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["schema"], "prometheus.project.v1");
    doc["project"].clone()
}

fn listed(home: &Path, extra: &[&str]) -> Vec<Value> {
    let mut args = vec!["--json", "projects", "list"];
    args.extend_from_slice(extra);
    let doc: Value = serde_json::from_str(&ok(&run(home, &args))).unwrap();
    assert_eq!(doc["schema"], "prometheus.projects_list.v1");
    doc["projects"].as_array().unwrap().clone()
}

fn slugs(projects: &[Value]) -> Vec<&str> {
    projects
        .iter()
        .map(|p| p["slug"].as_str().unwrap())
        .collect()
}

/// Scenario: Create and activate.
#[test]
fn create_with_use_lists_both_folders_and_marks_the_project_active() {
    let home = temp_home();
    let code = dir(home.path(), "src/prometheus");
    let docs = dir(home.path(), "src/docs");
    let slug = ok(&run(
        home.path(),
        &[
            "projects",
            "create",
            "Prometheus Agent",
            s(&code),
            s(&docs),
            "--use",
        ],
    ));
    assert_eq!(slug.trim(), "prometheus-agent");
    let project = show(home.path(), "prometheus-agent");
    assert_eq!(project["name"], "Prometheus Agent");
    assert_eq!(project["active"], true);
    assert_eq!(project["primary_path"], s(&code));
    let folders = project["folders"].as_array().unwrap();
    assert_eq!(folders.len(), 2);
    assert_eq!(folders[0]["path"], s(&code));
    assert_eq!(folders[0]["primary"], true);
    assert_eq!(folders[1]["path"], s(&docs));
    assert_eq!(folders[1]["primary"], false);
    // The text form shows the same.
    let text = ok(&run(home.path(), &["projects", "show", "prometheus-agent"]));
    assert!(text.contains(&format!("{}  (primary)", s(&code))), "{text}");
    assert!(text.contains("active       yes"), "{text}");
    let listing = ok(&run(home.path(), &["projects", "ls"]));
    assert!(listing.starts_with("* prometheus-agent"), "{listing}");
}

/// Scenario: Folder claimed twice.
#[test]
fn a_folder_of_one_project_cannot_join_another() {
    let home = temp_home();
    let api = dir(home.path(), "src/api");
    ok(&run(home.path(), &["projects", "create", "api", s(&api)]));
    ok(&run(home.path(), &["projects", "create", "web"]));
    let out = run(home.path(), &["projects", "add-folder", "web", s(&api)]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let expected = format!("folder {} already belongs to project 'api'", s(&api));
    assert!(stderr(&out).contains(&expected), "{}", stderr(&out));
    // Creating a project over it fails the same way and creates nothing.
    let out = run(home.path(), &["projects", "create", "other", s(&api)]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(slugs(&listed(home.path(), &[])), ["api", "web"]);
}

/// Scenario: Nested folders.
#[test]
fn a_session_belongs_to_the_innermost_project_of_its_directory() {
    let home = common::mock_home();
    let src = dir(home.path(), "src");
    let svc = dir(home.path(), "src/svc");
    let lib = dir(home.path(), "src/svc/lib");
    ok(&run(home.path(), &["projects", "create", "mono", s(&src)]));
    ok(&run(home.path(), &["projects", "create", "svc", s(&svc)]));
    let out = command(home.path())
        .current_dir(&lib)
        .args(["chat", "-z", "hi", "--provider", "mock"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    ok(&out);
    let sessions = |project: &str| -> Vec<Value> {
        let out = ok(&run(
            home.path(),
            &["--json", "sessions", "list", "--all", "--project", project],
        ));
        let doc: Value = serde_json::from_str(&out).unwrap();
        doc["sessions"].as_array().unwrap().clone()
    };
    let svc_sessions = sessions("svc");
    assert_eq!(svc_sessions.len(), 1);
    assert_eq!(
        svc_sessions[0]["project_id"],
        show(home.path(), "svc")["id"]
    );
    assert!(sessions("mono").is_empty());
    // A path component boundary: `/src/svc-old` is not inside `/src/svc`.
    let sibling = dir(home.path(), "src/svc-old");
    let out = command(home.path())
        .current_dir(&sibling)
        .args(["chat", "-z", "hi", "--provider", "mock"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    ok(&out);
    assert_eq!(sessions("mono").len(), 1);
    assert_eq!(sessions("svc").len(), 1);
}

/// Scenario: Archived active project cleared.
#[test]
fn archiving_the_active_project_clears_it_and_hides_it() {
    let home = temp_home();
    let api = dir(home.path(), "src/api");
    ok(&run(
        home.path(),
        &["projects", "create", "api", s(&api), "--use"],
    ));
    ok(&run(home.path(), &["projects", "archive", "api"]));
    assert!(listed(home.path(), &[]).is_empty());
    let text = ok(&run(home.path(), &["projects", "list"]));
    assert!(!text.contains("api"), "{text}");
    let all = listed(home.path(), &["--all"]);
    assert_eq!(slugs(&all), ["api"]);
    assert_eq!(all[0]["archived"], true);
    assert_eq!(all[0]["active"], false);
    // Activating an archived project is refused.
    let out = run(home.path(), &["projects", "use", "api"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
}

/// Archive releases the folders; restore fails once one was claimed.
#[test]
fn restore_fails_when_a_released_folder_was_claimed_meanwhile() {
    let home = temp_home();
    let api = dir(home.path(), "src/api");
    ok(&run(home.path(), &["projects", "create", "api", s(&api)]));
    ok(&run(home.path(), &["projects", "archive", "api"]));
    ok(&run(home.path(), &["projects", "create", "web", s(&api)]));
    let out = run(home.path(), &["projects", "restore", "api"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let expected = format!("folder {} now belongs to project 'web'", s(&api));
    assert!(stderr(&out).contains(&expected), "{}", stderr(&out));
    ok(&run(
        home.path(),
        &["projects", "remove-folder", "web", s(&api)],
    ));
    ok(&run(home.path(), &["projects", "restore", "api"]));
    assert_eq!(slugs(&listed(home.path(), &[])), ["api", "web"]);
    // The archived project kept its folder rows.
    assert_eq!(show(home.path(), "api")["primary_path"], s(&api));
}

#[test]
fn an_unknown_project_exits_66() {
    let home = temp_home();
    for args in [
        &["projects", "show", "nope"][..],
        &["projects", "use", "nope"],
        &["projects", "archive", "nope"],
        &["sessions", "list", "--project", "nope"],
    ] {
        let out = run(home.path(), args);
        assert_eq!(out.status.code(), Some(66), "{args:?}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("no project 'nope'"),
            "{}",
            stderr(&out)
        );
    }
}

#[test]
fn folders_names_boards_and_the_active_pointer_are_editable() {
    let home = temp_home();
    let one = dir(home.path(), "one");
    let two = dir(home.path(), "two");
    let primary = dir(home.path(), "main");
    ok(&run(
        home.path(),
        &[
            "projects",
            "create",
            "Tools",
            s(&one),
            "--primary",
            s(&primary),
            "--description",
            "dev tools",
            "--board",
            "backend",
        ],
    ));
    let project = show(home.path(), "tools");
    assert_eq!(project["primary_path"], s(&primary));
    assert_eq!(project["board"], "backend");
    assert_eq!(project["description"], "dev tools");
    ok(&run(
        home.path(),
        &[
            "projects",
            "add-folder",
            "tools",
            s(&two),
            "--label",
            "docs",
        ],
    ));
    // set-primary needs one of the project's folders.
    let out = run(
        home.path(),
        &[
            "projects",
            "set-primary",
            "tools",
            s(&home.path().join("nowhere")),
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    ok(&run(
        home.path(),
        &["projects", "set-primary", "tools", s(&two)],
    ));
    ok(&run(
        home.path(),
        &["projects", "rename", "tools", "Dev Tools"],
    ));
    ok(&run(home.path(), &["projects", "bind-board", "tools"]));
    // Removing the primary promotes the folder added first.
    ok(&run(
        home.path(),
        &["projects", "remove-folder", "tools", s(&two)],
    ));
    let project = show(home.path(), "tools");
    assert_eq!(project["name"], "Dev Tools");
    assert_eq!(project["slug"], "tools");
    assert_eq!(project["board"], Value::Null);
    assert_eq!(project["primary_path"], s(&primary));
    assert_eq!(project["folders"].as_array().unwrap().len(), 2);
    // The id resolves like the slug; `use` with no project clears.
    let id = project["id"].as_str().unwrap().to_owned();
    ok(&run(home.path(), &["projects", "use", &id]));
    assert_eq!(show(home.path(), "tools")["active"], true);
    ok(&run(home.path(), &["projects", "use"]));
    assert_eq!(show(home.path(), "tools")["active"], false);
    // A second project of the same name gets a numbered slug.
    let slug = ok(&run(home.path(), &["projects", "create", "Tools"]));
    assert_eq!(slug.trim(), "tools-2");
    let out = run(home.path(), &["projects", "create", "X", "--slug", "tools"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let out = run(
        home.path(),
        &["projects", "create", "X", "--slug", "Bad Slug"],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}
