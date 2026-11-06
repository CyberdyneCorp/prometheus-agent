//! Delegation through plugins and routes in real sessions on the mock
//! provider (spec: subagent-delegation, Delegation routers fail closed;
//! Profile routes run the child inside the target `ProfileScope`; Subagent
//! lifecycle interface for plugins; Child processes are reaped or handed
//! off).

mod common;

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use common::{append_config, command, mock_home, ok, run, run_with_stdin, stderr, stdout};
use serde_json::Value;

fn plugin_dir(home: &Path, id: &str) -> PathBuf {
    home.join("profiles/default/plugins").join(id)
}

/// Places plugin `id` (crate `component`) implementing `world` with
/// `[capabilities]` text and extra manifest tables, then enables it with
/// every grant it asks for.
fn install(home: &Path, id: &str, component: &str, world: &str, capabilities: &str, extra: &str) {
    let dir = plugin_dir(home, id);
    pmt_plugins::testkit::install(component, &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "manifest_version = 1\nid = \"{id}\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
             description = \"test\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
             worlds = [\"{world}\"]\n\n[capabilities]\n{capabilities}\n\n{extra}\n"
        ),
    )
    .unwrap();
    pmt_plugins::record::InstallRecord {
        source: pmt_plugins::record::Origin::Adopted,
        location: dir.to_string_lossy().into_owned(),
        r#ref: None,
        package_digest: None,
        digest: pmt_plugins::digest::of_dir(&dir).unwrap(),
        signer: None,
        tier: None,
        installed_at: "2026-11-06T00:00:00Z".to_owned(),
        flags: pmt_plugins::record::Flags::default(),
        held_update: None,
    }
    .write(&dir)
    .unwrap();
    ok(&run(home, &["plugins", "enable", id, "--grant", "all"]));
}

fn oneshot(home: &Path, prompt: &str, script: &str) -> Output {
    let path = home.join("script.toml");
    std::fs::write(&path, script).unwrap();
    command(home)
        .current_dir(home)
        .args(["--json", "-z", prompt, "--mock-script"])
        .arg(&path)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn answer(out: &Output) -> String {
    assert!(out.status.success(), "{}", stderr(out));
    let body: Value = serde_json::from_str(stdout(out).trim())
        .unwrap_or_else(|e| panic!("{e}: stdout {} stderr {}", stdout(out), stderr(out)));
    body["answer"].as_str().unwrap_or_default().to_owned()
}

fn subagent_sessions(home: &Path) -> usize {
    let listed = ok(&run(home, &["--json", "sessions", "list", "--all"]));
    let listed: Value = serde_json::from_str(&listed).unwrap();
    listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["source"] == "subagent")
        .count()
}

/// A script whose first call is the probe tool `tool` with `v`, and which
/// answers with whatever result came back. `child` rules answer the
/// child.
fn probe_script(tool: &str, v: &str, child: &str) -> String {
    format!(
        r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "{tool}"
arguments = {{ v = "{v}" }}

{child}

[[rule]]
tool_result_regex = "(?s)(.*)"
repeat = true
text = "parent saw it"
"#
    )
}

/// The JSON object inside a tool result's text.
fn json_in(text: &str) -> Value {
    let (start, end) = (text.find('{'), text.rfind('}'));
    match (start, end) {
        (Some(start), Some(end)) => {
            serde_json::from_str(&text[start..=end]).unwrap_or_else(|e| panic!("{e}: {text}"))
        }
        _ => panic!("no JSON in {text}"),
    }
}

/// The text of the last tool result of the session the one-shot run
/// `out` reports.
fn last_tool_result(home: &Path, out: &Output) -> String {
    let body: Value = serde_json::from_str(stdout(out).trim()).unwrap();
    let id = body["session_id"].as_str().unwrap();
    common::session_messages(home, id)
        .iter()
        .rev()
        .find(|m| m["role"] == "tool")
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// Scenario: Plugin without the capability.
#[test]
fn a_plugin_without_the_subagents_capability_spawns_no_child() {
    let home = mock_home();
    install(home.path(), "probe", "testing-probe", "tool@1.2", "", "");
    let script = probe_script("subagent", "Map the parser", "");
    let out = oneshot(home.path(), "go", &script);
    assert_eq!(answer(&out), "parent saw it");
    let result = last_tool_result(home.path(), &out);
    assert!(result.contains("capability_denied: subagents"), "{result}");
    assert_eq!(subagent_sessions(home.path()), 0);
}

/// A plugin holding `subagents` launches a child of the calling session
/// through the same path as `delegate_task`, waits for it and reads its
/// result.
#[test]
fn a_plugin_with_the_capability_launches_and_waits_for_a_child() {
    let home = mock_home();
    install(
        home.path(),
        "probe",
        "testing-probe",
        "tool@1.2",
        "subagents = true",
        "",
    );
    let child = r#"
[[rule]]
user = "Map the parser"
text = "the parser has three stages"
"#;
    let script = probe_script("subagent", "Map the parser", child);
    let out = oneshot(home.path(), "go", &script);
    assert_eq!(answer(&out), "parent saw it");
    let result = last_tool_result(home.path(), &out);
    assert!(result.contains("State::Completed|"), "{result}");
    assert!(result.contains("the parser has three stages"), "{result}");
    assert_eq!(subagent_sessions(home.path()), 1);
}

/// Scenario: Forged handle.
#[test]
fn a_forged_handle_is_unknown_to_the_plugin_interface() {
    let home = mock_home();
    install(
        home.path(),
        "probe",
        "testing-probe",
        "tool@1.2",
        "subagents = true",
        "",
    );
    let child = r#"
[[rule]]
user = "Map the parser"
text = "mapped"
"#;
    let script = probe_script("forged_handle", "Map the parser", child);
    let out = oneshot(home.path(), "go", &script);
    assert_eq!(answer(&out), "parent saw it");
    let result = last_tool_result(home.path(), &out);
    assert!(result.contains("unknown_handle"), "{result}");
}

/// Scenario: Missing router.
#[test]
fn a_router_no_plugin_provides_fails_every_spawn() {
    let home = mock_home();
    append_config(home.path(), "\n[delegation]\nrouter = \"tiers\"\n");
    let script = r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "delegate_task"
arguments = { tasks = [{ goal = "Summarise the README" }] }

[[rule]]
tool_result_regex = "(?s)(.*)"
repeat = true
text = "parent saw it"
"#;
    let out = oneshot(home.path(), "go", script);
    assert_eq!(answer(&out), "parent saw it");
    let result = last_tool_result(home.path(), &out);
    assert!(
        result.contains("Delegation router 'tiers' is unavailable"),
        "{result}"
    );
    assert_eq!(subagent_sessions(home.path()), 0);
}

const ROUTER: &str = "[delegation_router]\nname = \"tiers\"\n\n[delegation_router.params]\nprofile = { type = \"string\" }\n";

/// Scenario: One unroutable task (with a plugin router).
#[test]
fn a_plugin_router_refuses_one_task_while_its_siblings_run() {
    let home = mock_home();
    install(
        home.path(),
        "router",
        "testing-router",
        "delegation-router@1.0",
        "",
        ROUTER,
    );
    append_config(home.path(), "\n[delegation]\nrouter = \"tiers\"\n");
    let script = r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "delegate_task"
arguments = { tasks = [{ goal = "Read the docs" }, { goal = "Please refuse this one" }, { goal = "Read the tests" }] }

[[rule]]
user_regex = "^Read the"
repeat = true
text = "read it"

[[rule]]
tool_result_regex = "(?s)(.*)"
repeat = true
text = "parent saw it"
"#;
    let out = oneshot(home.path(), "go", script);
    assert_eq!(answer(&out), "parent saw it");
    let result: Value = json_in(&last_tool_result(home.path(), &out));
    let statuses: Vec<&str> = result["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, ["completed", "refused", "completed"], "{result}");
    assert!(
        result.to_string().contains("no tier fits"),
        "the router's message: {result}"
    );
    assert_eq!(subagent_sessions(home.path()), 2);
}

/// Scenario: No launching-profile secret leak.
#[test]
fn a_child_routed_to_another_profile_never_sees_the_launching_profiles_secret() {
    let home = mock_home();
    ok(&run(home.path(), &["profile", "create", "work"]));
    run_with_stdin(
        home.path(),
        &["secrets", "set", "OPENROUTER_API_KEY"],
        "sk-or-launching-profile\n",
    );
    install(
        home.path(),
        "router",
        "testing-router",
        "delegation-router@1.0",
        "",
        ROUTER,
    );
    append_config(home.path(), "\n[delegation]\nrouter = \"tiers\"\n");
    let script = r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "delegate_task"
arguments = { tasks = [{ goal = "Audit the build", profile = "work" }] }

[[rule]]
tool_result_regex = "(?s)(.*)"
repeat = true
text = "parent saw it"
"#;
    let out = oneshot(home.path(), "go", script);
    assert_eq!(answer(&out), "parent saw it");
    let result: Value = json_in(&last_tool_result(home.path(), &out));
    let child = &result["results"][0];
    assert_eq!(child["status"], "failed", "{result}");
    assert_eq!(
        child["error"]["code"], "provider.missing_credential",
        "{result}"
    );
}

/// Scenario: Handed-off process survives the child.
#[cfg(unix)]
#[test]
fn a_handed_off_process_outlives_the_child_and_belongs_to_the_parent() {
    let home = mock_home();
    // The first rule answers the parent once its own `process wait` saw
    // the server end (after the child had finished).
    let script = r#"
[[rule]]
tool_result = "stopped-42"
text = "the parent saw the server stop"

[[rule]]
user = "go"
[[rule.tool_calls]]
name = "delegate_task"
arguments = { tasks = [{ goal = "Start the dev server" }] }

[[rule]]
user = "Start the dev server"
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "sleep 4; echo stopped-$((40 + 2))", background = true }

[[rule]]
tool_result_regex = "proc_"
[[rule.tool_calls]]
name = "process"
arguments = { action = "handoff", process_id = "proc_" }

[[rule]]
tool_result = "handed_off"
text = "server handed to the parent"

[[rule]]
tool_result = "server handed to the parent"
[[rule.tool_calls]]
name = "process"
arguments = { action = "wait", process_id = "proc_", timeout = 30 }
"#;
    let out = oneshot(home.path(), "go", script);
    assert_eq!(answer(&out), "the parent saw the server stop");
    let waited = last_tool_result(home.path(), &out);
    assert!(waited.contains("\"exit_code\":0"), "{waited}");
}
