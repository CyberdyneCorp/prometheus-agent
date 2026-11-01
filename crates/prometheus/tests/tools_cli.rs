//! `prometheus tools` and `prometheus toolsets` through the binary (spec:
//! tool-system, Tool introspection commands; Toolset listing commands;
//! Core toolset membership), compared with snapshots in `tests/snapshots/`.

mod common;

use std::path::Path;

use common::{ok, run, stderr, temp_home};
use jiff::Timestamp;
use pmt_store::tool_surface::{RecordedSurface, SurfaceTool};
use pmt_store::{NewSession, Source, Store};
use serde_json::Value;

fn snapshot(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(name);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, actual).unwrap();
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing snapshot {name}; run with UPDATE_SNAPSHOTS=1"));
    assert_eq!(actual, expected, "snapshot {name}");
}

fn json(home: &Path, args: &[&str]) -> Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    serde_json::from_str(&ok(&run(home, &full))).unwrap()
}

#[test]
fn toolsets_list_marks_active_default_off_and_posture() {
    let home = temp_home();
    snapshot(
        "toolsets-list.txt",
        &ok(&run(home.path(), &["toolsets", "list"])),
    );
    let doc = json(home.path(), &["toolsets", "list"]);
    assert_eq!(doc["schema"], "prometheus.toolsets.v1");
    let toolsets = doc["toolsets"].as_array().unwrap();
    let get = |name: &str| toolsets.iter().find(|t| t["name"] == name).unwrap().clone();
    assert_eq!(get("core")["active"], true);
    assert_eq!(get("core")["resolved"], 27);
    assert_eq!(get("web")["active"], false);
    assert_eq!(get("video")["default_off"], true);
    assert_eq!(get("coding")["posture"], true);
    assert_eq!(
        get("safe")["includes"],
        serde_json::json!(["web", "vision", "image_gen"])
    );
    // `tools toolsets` is the same listing.
    assert_eq!(json(home.path(), &["tools", "toolsets"]), doc);
}

#[test]
fn surface_listing_follows_the_surface_config() {
    let home = temp_home();
    let set = ["--set", "toolsets.surfaces.cron=[\"web\", \"file\"]"];
    let mut args = set.to_vec();
    args.extend(["toolsets", "list", "--surface", "cron"]);
    let doc = json(home.path(), &args);
    let active: Vec<&str> = doc["toolsets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["active"] == true)
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(active, ["file", "web"]);
}

#[test]
fn core_listing_shows_the_27_tools_in_order() {
    let home = temp_home();
    let text = ok(&run(home.path(), &["toolsets", "show", "core"]));
    snapshot("toolsets-show-core.txt", &text);
    let doc = json(home.path(), &["toolsets", "show", "core"]);
    assert_eq!(doc["schema"], "prometheus.toolset.v1");
    let names: Vec<&str> = doc["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, pmt_tools::toolset::CORE);
}

#[test]
fn unknown_names_exit_2_with_the_closest_names() {
    let home = temp_home();
    let out = run(home.path(), &["toolsets", "show", "brwser"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).starts_with("error: unknown toolset 'brwser' (did you mean 'browser'?)"),
        "{}",
        stderr(&out)
    );
    let out = run(home.path(), &["tools", "explain", "todo_list"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        stderr(&out).lines().next(),
        Some("error: unknown tool: todo_list")
    );
}

#[test]
fn tools_list_is_a_versioned_document() {
    let home = temp_home();
    let doc = json(home.path(), &["tools", "list"]);
    assert_eq!(doc["schema"], "prometheus.tools.v1");
    assert!(doc["tools"].is_array());
}

/// A session in the temp home that recorded `tools`, titled `title`.
fn session_with_surface(home: &Path, title: &str, tools: Option<Vec<SurfaceTool>>) -> String {
    let home = pmt_config::Home::open(home).unwrap();
    let profile = home
        .profile(&pmt_config::ProfileName::default_profile())
        .unwrap();
    let scope = pmt_config::ProfileScope::open(
        profile,
        &pmt_config::Overrides::default(),
        pmt_config::Launch::Other,
    )
    .unwrap();
    let store = Store::open(&scope).unwrap();
    let mut new = NewSession::new(Source::Cli);
    new.title = Some(title.to_owned());
    let session = store.create_session(&new).unwrap();
    if let Some(tools) = tools {
        let surface = RecordedSurface {
            generation: 1,
            recorded_at: Timestamp::from_millisecond(1_700_000_000_000).unwrap(),
            tools,
        };
        store.record_tool_surface(session.id, &surface).unwrap();
    }
    session.id.to_string()
}

fn row(name: &str, toolset: &str, exposure: &str) -> SurfaceTool {
    SurfaceTool {
        name: name.into(),
        toolset: toolset.into(),
        owner: "builtin".into(),
        exposure: exposure.into(),
    }
}

#[test]
fn tools_list_session_shows_the_frozen_surface() {
    let home = temp_home();
    let id = session_with_surface(
        home.path(),
        "refactor",
        Some(vec![
            row("terminal", "terminal", "exposed"),
            row("browser_exec", "browser", "unavailable:browser not found"),
        ]),
    );
    let text = ok(&run(
        home.path(),
        &["tools", "list", "--session", "refactor"],
    ));
    snapshot("tools-list-session.txt", &text.replace(&id, "<SESSION>"));

    let doc = json(home.path(), &["tools", "list", "--session", &id]);
    assert_eq!(doc["schema"], "prometheus.tools.v1");
    assert_eq!(doc["session"], id.as_str());
    assert_eq!(doc["generation"], 1);
    assert_eq!(doc["tools"][1]["exposure"], "unavailable:browser not found");
}

#[test]
fn tools_list_session_without_a_surface_or_session_exits_66() {
    let home = temp_home();
    session_with_surface(home.path(), "imported", None);
    let out = run(home.path(), &["tools", "list", "--session", "imported"]);
    assert_eq!(out.status.code(), Some(66));
    assert!(
        stderr(&out).contains("recorded no tool surface"),
        "{}",
        stderr(&out)
    );
    let out = run(home.path(), &["tools", "list", "--session", "nope"]);
    assert_eq!(out.status.code(), Some(66));
}
