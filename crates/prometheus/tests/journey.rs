//! The learning journey through the binary (spec: memory, Learning journey
//! timeline; Journey pruning and editing): the agent creates a skill and
//! the user remembers two facts; `prometheus journey` shows, lists, prunes
//! and edits them, `/journey` does the same in a chat and `journey.list`
//! serves the nodes to daemon clients.

mod common;

use std::path::Path;

use common::{command, mock_home, ok, run, stderr};
use serde_json::Value;

/// The agent creates `deploy-checklist` with `skill_manage`.
const SCRIPT: &str = r#"
[[rule]]
user = "learn how we deploy"
times = 1
[[rule.tool_calls]]
name = "skill_manage"
arguments = { action = "create", name = "deploy-checklist", content = "---\nname: deploy-checklist\ndescription: Steps before a production deploy.\n---\n\nRun the tests, then tag the release.\n" }

[[rule]]
repeat = true
text = "learned"
"#;

/// A home whose profile has one agent-created skill and two memory
/// entries; the entries' ids.
fn learned_home() -> (tempfile::TempDir, Vec<String>) {
    let home = mock_home();
    let script = home.path().join("script.toml");
    std::fs::write(&script, SCRIPT).unwrap();
    let out = run(
        home.path(),
        &[
            "chat",
            "--mock-script",
            script.to_str().unwrap(),
            "-z",
            "learn how we deploy",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let ids = ["Prefers metric units.", "Works in UTC."]
        .iter()
        .map(|fact| {
            let added = ok(&run(
                home.path(),
                &["--json", "memory", "add", "--target", "user", fact],
            ));
            serde_json::from_str::<Value>(&added).unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    (home, ids)
}

/// The nodes `--json <args>` prints.
fn nodes(home: &Path, args: &[&str]) -> Vec<Value> {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    serde_json::from_str(&ok(&run(home, &full))).unwrap()
}

/// Scenario: JSON nodes.
#[test]
fn json_nodes_hold_the_learned_skill_and_both_entries() {
    let (home, ids) = learned_home();
    let all = nodes(home.path(), &["journey"]);
    assert_eq!(all.len(), 3, "{all:?}");
    let skill = all
        .iter()
        .find(|n| n["id"] == "skill:deploy-checklist")
        .expect("the skill node");
    assert_eq!(skill["kind"], "skill");
    assert_eq!(skill["origin"], "agent");
    let memories: Vec<&Value> = all
        .iter()
        .filter(|n| n["id"].as_str().unwrap().starts_with("m_"))
        .collect();
    assert_eq!(memories.len(), 2, "{all:?}");
    for node in &memories {
        assert!(ids.contains(&node["id"].as_str().unwrap().to_owned()));
        for key in ["title", "created_at", "updated_at", "origin"] {
            assert!(node[key].is_string(), "{key} in {node}");
        }
    }
    let created: Vec<&str> = all
        .iter()
        .map(|n| n["created_at"].as_str().unwrap())
        .collect();
    assert!(created.windows(2).all(|w| w[0] <= w[1]), "oldest first");
    for alias in ["learning", "memory-graph"] {
        assert_eq!(nodes(home.path(), &[alias]).len(), 3, "{alias}");
    }
    let future = nodes(home.path(), &["journey", "--since", "2999-01-01"]);
    assert!(future.is_empty(), "{future:?}");
}

/// The timeline and `list` name every node; `--play` off a terminal shows
/// the last frame.
#[test]
fn the_timeline_and_the_listing_show_every_node() {
    let (home, ids) = learned_home();
    let view = ok(&run(
        home.path(),
        &["journey", "--width", "60", "--height", "4"],
    ));
    assert!(
        view.starts_with("Learning journey: 1 skill, 2 memories"),
        "{view}"
    );
    assert!(view.contains("deploy-checklist"), "{view}");
    assert!(view.lines().all(|l| l.chars().count() <= 60), "{view}");
    assert!(!view.contains('\x1b'), "no color off a terminal: {view}");
    let played = ok(&run(home.path(), &["journey", "--play", "--fps", "60"]));
    assert!(played.contains("1 skill, 2 memories"), "{played}");
    let listed = ok(&run(home.path(), &["journey", "list"]));
    for id in ids
        .iter()
        .map(String::as_str)
        .chain(["skill:deploy-checklist"])
    {
        assert!(listed.contains(id), "{id} in {listed}");
    }
}

/// Scenario: Delete a memory node.
#[test]
fn deleting_a_memory_node_is_a_revertible_user_revision() {
    let (home, ids) = learned_home();
    let id = &ids[0];
    let out = run(home.path(), &["journey", "delete", id, "--yes"]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    let entries = ok(&run(home.path(), &["--json", "memory", "list"]));
    assert!(!entries.contains(id.as_str()), "removed: {entries}");
    let log = ok(&run(home.path(), &["memory", "log", "--origin", "user"]));
    let line = log
        .lines()
        .find(|l| l.contains(id.as_str()) && l.contains("remove"))
        .unwrap_or_else(|| panic!("a user revision removes it: {log}"));
    let rev = err
        .split("revision ")
        .nth(1)
        .and_then(|r| r.split(';').next())
        .unwrap_or_else(|| panic!("the revision is named: {err} / {line}"));
    ok(&run(home.path(), &["memory", "revert", rev]));
    let entries = ok(&run(home.path(), &["--json", "memory", "list"]));
    assert!(entries.contains(id.as_str()), "restored: {entries}");
}

/// Deleting without `--yes` and without a terminal changes nothing and
/// exits 4.
#[test]
fn deleting_without_yes_off_a_terminal_exits_4() {
    let (home, ids) = learned_home();
    for node in [ids[0].as_str(), "skill:deploy-checklist"] {
        let out = command(home.path())
            .args(["journey", "delete", node])
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
        assert!(stderr(&out).contains("--yes"), "{}", stderr(&out));
    }
    assert_eq!(nodes(home.path(), &["journey"]).len(), 3);
}

/// Deleting a skill node archives the skill: it leaves the journey and the
/// catalog and its files wait under `skills/.archive/`.
#[test]
fn deleting_a_skill_node_archives_the_skill() {
    let (home, _) = learned_home();
    let out = run(
        home.path(),
        &["journey", "delete", "skill:deploy-checklist", "--yes"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let skills = home.path().join("profiles/default/skills");
    assert!(!skills.join("deploy-checklist").exists());
    let archived: Vec<_> = std::fs::read_dir(skills.join(".archive"))
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("deploy-checklist-")
        })
        .collect();
    assert_eq!(archived.len(), 1);
    assert!(archived[0].path().join("SKILL.md").is_file());
    let left = nodes(home.path(), &["journey"]);
    assert!(left.iter().all(|n| n["kind"] == "memory"), "{left:?}");
}

#[cfg(unix)]
fn editor(home: &Path, sed: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let script = home.join("editor.sh");
    let body = format!("#!/bin/sh\nsed -e '{sed}' \"$1\" > \"$1.tmp\" && mv \"$1.tmp\" \"$1\"\n");
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// `journey edit` opens the entry or the skill in `$EDITOR` and applies
/// it through the usual checks.
#[cfg(unix)]
#[test]
fn editing_nodes_goes_through_the_editor_and_the_usual_checks() {
    let (home, ids) = learned_home();
    let edit = |node: &str, sed: &str| {
        command(home.path())
            .env("EDITOR", editor(home.path(), sed))
            .args(["journey", "edit", node])
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    };
    let out = edit(&ids[0], "s/metric units/metric units and UTC/");
    assert!(out.status.success(), "{}", stderr(&out));
    let shown = ok(&run(home.path(), &["--json", "memory", "show", &ids[0]]));
    assert!(shown.contains("Prefers metric units and UTC."), "{shown}");
    let out = edit(
        "skill:deploy-checklist",
        "s/tag the release/tag and sign the release/",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let skill = std::fs::read_to_string(
        home.path()
            .join("profiles/default/skills/deploy-checklist/SKILL.md"),
    )
    .unwrap();
    assert!(skill.contains("tag and sign the release"), "{skill}");
    // A broken SKILL.md is refused and the file stays as it was.
    let out = edit("skill:deploy-checklist", "s/^name: .*$//");
    assert!(!out.status.success(), "{}", stderr(&out));
    let after = std::fs::read_to_string(
        home.path()
            .join("profiles/default/skills/deploy-checklist/SKILL.md"),
    )
    .unwrap();
    assert_eq!(after, skill);
}

/// `/journey` in a messaging chat lists the nodes, and `journey.list`
/// serves them to daemon clients.
#[cfg(feature = "gateway")]
#[test]
fn a_chat_and_a_daemon_client_see_the_same_journey() {
    use common::gateway::{Daemon, send, wait_said};
    let (home, ids) = learned_home();
    let home = home.path();
    common::append_config(
        home,
        "\n[gateway.busy]\ndebounce_ms = 0\n\n[gateway.platforms.test]\nenabled = true\nallow_all = true\n",
    );
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "/journey list");
    wait_said(home, "default", "skill:deploy-checklist");
    wait_said(home, "default", &ids[1]);
    let listed: Value =
        serde_json::from_str(&ok(&run(home, &["rpc", "call", "journey.list"]))).unwrap();
    let served = listed["nodes"].as_array().unwrap();
    assert_eq!(served.len(), 3, "{listed}");
    assert!(served.iter().any(|n| n["id"] == "skill:deploy-checklist"));
    send(
        home,
        "default",
        "u1",
        &format!("/journey delete {}", ids[1]),
    );
    wait_said(home, "default", "--yes to do it");
    send(
        home,
        "default",
        "u1",
        &format!("/journey delete {} --yes", ids[1]),
    );
    wait_said(home, "default", &format!("removed {}", ids[1]));
    daemon.stop(home);
}
