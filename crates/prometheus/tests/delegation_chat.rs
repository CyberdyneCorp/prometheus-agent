//! `delegate_task` in real sessions on the mock provider (spec:
//! subagent-delegation): children run as isolated `subagent` sessions
//! under a grant that only narrows, on their own model route, with
//! approvals that fail closed.

mod common;

use std::path::Path;
use std::process::Output;

use common::{append_config, command, mock_home, ok, run, stderr, stdout};
use serde_json::Value;

fn oneshot(home: &Path, prompt: &str, script: &str) -> Output {
    let path = home.join("script.toml");
    std::fs::write(&path, script).unwrap();
    command(home)
        .current_dir(home)
        .args(["--json", "-z", prompt, "--mock-script"])
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn answer(out: &Output) -> String {
    let body: Value = serde_json::from_str(stdout(out).trim())
        .unwrap_or_else(|e| panic!("{e}: stdout {} stderr {}", stdout(out), stderr(out)));
    body["answer"].as_str().unwrap_or_default().to_owned()
}

fn sessions(home: &Path) -> Vec<Value> {
    let listed = ok(&run(home, &["--json", "sessions", "list", "--all"]));
    let listed: Value = serde_json::from_str(&listed).unwrap();
    listed["sessions"].as_array().unwrap().clone()
}

/// The parent delegates `task` (a TOML inline table) on its first call
/// and answers with the tool result it got.
fn delegating(task: &str, child_rules: &str) -> String {
    format!(
        r#"
[[rule]]
user = "please delegate"
[[rule.tool_calls]]
name = "delegate_task"
arguments = {{ tasks = [{task}] }}

{child_rules}

[[rule]]
tool_result_regex = "(?s).*"
repeat = true
text = "parent saw the result"
"#
    )
}

/// Scenario: Broadening refused (end to end): a child cannot gain a tool
/// its parent lacks.
#[test]
fn a_child_cannot_gain_a_toolset_its_parent_lacks() {
    let home = mock_home();
    append_config(
        home.path(),
        "[tools]\nenabled_toolsets = [\"file\", \"delegation\"]\n",
    );
    let script = format!(
        "{}\n[[rule]]\ntool_result = \"grant_broadening\"\ntext = \"refused: broadening\"\n",
        r#"
[[rule]]
user = "please delegate"
[[rule.tool_calls]]
name = "delegate_task"
arguments = { tasks = [{ goal = "Open the docs site", toolsets = ["browser"] }] }

[[rule]]
tool_result_regex = "browser_exec.*browser_cdp|browser_cdp.*browser_exec"
text = "refused with every browser tool listed"
"#
    );
    let out = oneshot(home.path(), "please delegate", &script);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(answer(&out), "refused with every browser tool listed");
    assert_eq!(sessions(home.path()).len(), 1, "no child session was built");
}

/// Scenario: Single task batch; Flat default yields leaf children; Child
/// cannot write memory.
#[test]
fn a_leaf_child_runs_without_delegation_memory_or_clarify() {
    let home = mock_home();
    let script = delegating(
        r#"{ goal = "Summarise the README" }"#,
        r#"
[[rule]]
user = "Summarise the README"
tools_regex = "\"(delegate_task|memory|clarify)\""
text = "CHILD SAW A FORBIDDEN TOOL"

[[rule]]
user = "Summarise the README"
text = "child summary done"
"#,
    );
    let out = oneshot(home.path(), "please delegate", &script);
    assert!(out.status.success(), "{}", stderr(&out));
    let listed = sessions(home.path());
    assert_eq!(listed.len(), 2, "exactly one child: {listed:?}");
    let child = listed
        .iter()
        .find(|s| s["source"] == "subagent")
        .expect("a subagent session");
    let messages = common::session_messages(home.path(), child["id"].as_str().unwrap());
    let texts: Vec<String> = messages
        .iter()
        .filter_map(|m| m["content"].as_str().map(str::to_owned))
        .collect();
    assert!(texts.iter().any(|t| t == "child summary done"), "{texts:?}");
    assert!(!texts.iter().any(|t| t.contains("FORBIDDEN")), "{texts:?}");
    // Scenario: Parent context untouched: progress events never enter the
    // parent's transcript (only the tool result does).
    let parent = listed.iter().find(|s| s["source"] != "subagent").unwrap();
    let parent_messages = common::session_messages(home.path(), parent["id"].as_str().unwrap());
    assert!(
        !parent_messages.iter().any(|m| {
            m["content"]
                .as_str()
                .is_some_and(|c| c.contains("subagent.progress") || c.contains("\"activity\""))
        }),
        "{parent_messages:?}"
    );
}

/// Scenario: Default deny: a child's command that needs approval is
/// denied even when the parent's one-shot run would approve it.
#[test]
fn a_childs_approval_is_denied_and_logged_with_its_id() {
    let home = mock_home();
    append_config(
        home.path(),
        "[approvals]\nsingle_query_mode = \"approve\"\n",
    );
    let script = delegating(
        r#"{ goal = "Clean the build directory", toolsets = ["terminal"] }"#,
        r#"
[[rule]]
user = "Clean the build directory"
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "rm -rf build" }

[[rule]]
tool_result_regex = "DENIED"
text = "the child was denied"
"#,
    );
    std::fs::create_dir_all(home.path().join("build")).unwrap();
    let out = oneshot(home.path(), "please delegate", &script);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        home.path().join("build").exists(),
        "the denied command did not run"
    );
    let child = sessions(home.path())
        .into_iter()
        .find(|s| s["source"] == "subagent")
        .expect("a child");
    let messages = common::session_messages(home.path(), child["id"].as_str().unwrap());
    assert!(
        messages
            .iter()
            .any(|m| m["content"].as_str() == Some("the child was denied")),
        "{messages:?}"
    );
    let logs = std::fs::read_to_string(home.path().join("profiles/default/logs/prometheus.log"))
        .unwrap_or_default();
    assert!(
        logs.contains("subagent approval denied") && logs.contains("sa-"),
        "{logs}"
    );
}

/// Scenario: Pinned child does not inherit fallback.
#[test]
fn a_pinned_child_fails_on_its_own_route_without_the_parents_chain() {
    let home = mock_home();
    append_config(
        home.path(),
        "[agent]\napi_max_retries = 1\nauto_recovery_cycles = 0\n\n\
         [[fallback.chain]]\nprovider = \"mock\"\nmodel = \"backup\"\n",
    );
    let script = delegating(
        r#"{ goal = "Cheap lookup", model = "mock/cheap-model" }"#,
        r#"
[[rule]]
model = "cheap-model"
fault = "server_error"
status = 529
repeat = true

[[rule]]
model = "backup"
user = "Cheap lookup"
text = "BACKUP ANSWERED THE CHILD"
"#,
    );
    let out = oneshot(home.path(), "please delegate", &script);
    assert!(out.status.success(), "{}", stderr(&out));
    let child = sessions(home.path())
        .into_iter()
        .find(|s| s["source"] == "subagent")
        .expect("a child");
    assert_eq!(child["model"], "cheap-model", "{child}");
    let messages = common::session_messages(home.path(), child["id"].as_str().unwrap());
    assert!(
        !messages
            .iter()
            .any(|m| m["content"].as_str() == Some("BACKUP ANSWERED THE CHILD")),
        "{messages:?}"
    );
}

/// Named agent definitions route their children and unknown names are
/// refused.
#[test]
fn a_named_agent_runs_on_its_model_and_unknown_agents_are_refused() {
    let home = mock_home();
    let agents = home.path().join("profiles/default/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("reviewer.md"),
        "---\nname: reviewer\ndescription: Reviews diffs\nmodel: mock/reviewer\n---\nYou review code strictly.\n",
    )
    .unwrap();
    let script = delegating(
        r#"{ goal = "Review the diff", agent = "reviewer" }, { goal = "Critique it", agent = "critic" }"#,
        r#"
[[rule]]
model = "reviewer"
text = "reviewed by the reviewer"
"#,
    );
    let out = oneshot(home.path(), "please delegate", &script);
    assert!(out.status.success(), "{}", stderr(&out));
    // The unknown agent refuses the whole call before any child is built.
    assert_eq!(sessions(home.path()).len(), 1);
    let script = delegating(
        r#"{ goal = "Review the diff", agent = "reviewer" }"#,
        r#"
[[rule]]
model = "reviewer"
text = "reviewed by the reviewer"
"#,
    );
    let out = oneshot(home.path(), "please delegate", &script);
    assert!(out.status.success(), "{}", stderr(&out));
    let child = sessions(home.path())
        .into_iter()
        .find(|s| s["source"] == "subagent")
        .expect("a child");
    assert_eq!(child["model"], "reviewer");
}

/// Scenario: Child cost appears in the root session total.
#[test]
fn a_childs_usage_rolls_up_into_the_root_session() {
    let home = mock_home();
    let script = delegating(
        r#"{ goal = "Count the files" }, { goal = "Count the lines" }"#,
        r#"
[[rule]]
user_regex = "^Count the"
repeat = true
text = "counted"
"#,
    );
    let out = oneshot(home.path(), "please delegate", &script);
    assert!(out.status.success(), "{}", stderr(&out));
    let listed = sessions(home.path());
    let root = listed.iter().find(|s| s["source"] != "subagent").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let usage = ok(&run(home.path(), &["--json", "usage", "--session", &root]));
    let usage: Value = serde_json::from_str(&usage).unwrap();
    // The parent's two calls and one call per child.
    assert_eq!(usage["total"]["calls"], 4, "{usage}");
}

/// Scenario: Child inherits the parent's filesystem limits.
#[test]
fn a_child_cannot_write_outside_its_parents_grant() {
    let home = mock_home();
    let outside = common::temp_home();
    let target = outside.path().join("hosts");
    let task = format!(
        r#"{{ goal = "Edit the hosts file", toolsets = ["file"], context = {:?} }}"#,
        target.to_string_lossy()
    );
    let child = format!(
        r#"
[[rule]]
user = "Edit the hosts file"
[[rule.tool_calls]]
name = "write_file"
arguments = {{ path = {:?}, content = "0.0.0.0 example.com\n" }}

[[rule]]
tool_result_regex = "not granted|outside|denied"
text = "the write was refused"
"#,
        target.to_string_lossy()
    );
    let out = oneshot(home.path(), "please delegate", &delegating(&task, &child));
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!target.exists(), "the child wrote outside the grant");
    let child = sessions(home.path())
        .into_iter()
        .find(|s| s["source"] == "subagent")
        .expect("a child");
    let messages = common::session_messages(home.path(), child["id"].as_str().unwrap());
    assert!(
        messages
            .iter()
            .any(|m| m["content"].as_str() == Some("the write was refused")),
        "{messages:?}"
    );
}

/// Scenario: Artifacts reported.
#[test]
fn a_childs_written_files_are_reported_with_their_digests() {
    let home = mock_home();
    let child = r##"
[[rule]]
user = "Write the crate skeleton"
[[rule.tool_calls]]
name = "write_file"
arguments = { path = "src/lib.rs", content = "pub fn answer() -> u32 { 42 }\n" }
[[rule.tool_calls]]
name = "write_file"
arguments = { path = "README.md", content = "# skeleton\n" }

[[rule]]
user = "Write the crate skeleton"
text = "wrote both files"
"##;
    let script = delegating(
        r#"{ goal = "Write the crate skeleton", toolsets = ["file"] }"#,
        child,
    );
    let out = oneshot(home.path(), "please delegate", &script);
    assert!(out.status.success(), "{}", stderr(&out));
    let parent = sessions(home.path())
        .into_iter()
        .find(|s| s["source"] != "subagent")
        .unwrap();
    let messages = common::session_messages(home.path(), parent["id"].as_str().unwrap());
    let result = messages
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["content"].as_str())
        .find(|c| c.contains("artifacts"))
        .expect("the delegate_task result");
    let start = result.find('{').unwrap();
    let end = result.rfind('}').unwrap();
    let body: Value = serde_json::from_str(&result[start..=end]).unwrap();
    let artifacts = body["results"][0]["artifacts"].as_array().unwrap();
    let paths: Vec<&str> = artifacts
        .iter()
        .map(|a| a["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["src/lib.rs", "README.md"]);
    for artifact in artifacts {
        let digest = artifact["sha256"].as_str().unwrap();
        assert_eq!(digest.len(), 64);
        assert_eq!(artifact["blob"], format!("blobs/{digest}"));
    }
}

/// Scenario: Even split enforced: each child's share of the call budget
/// is a hard limit of its own; the child that reaches it stops with
/// `budget_exhausted` while its sibling finishes.
#[test]
fn a_child_that_spends_its_share_stops_while_its_sibling_finishes() {
    let home = mock_home();
    let script = r#"
[[rule]]
user = "please delegate"
[[rule.tool_calls]]
name = "delegate_task"
arguments = { tasks = [{ goal = "Busy work" }, { goal = "Quick answer" }], budget = { max_tokens = 20000 } }

[[rule]]
user = "Busy work"
repeat = true
[[rule.tool_calls]]
name = "todo"
arguments = { todos = [{ id = "1", content = "keep going", status = "pending" }] }

[[rule]]
user = "Quick answer"
text = "quick done"

[[rule]]
tool_result_regex = "(?s)results"
text = "parent saw the results"
"#;
    let out = oneshot(home.path(), "please delegate", script);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(answer(&out), "parent saw the results");
    let body: Value = serde_json::from_str(stdout(&out).trim()).unwrap();
    let parent = body["session_id"].as_str().unwrap();
    let messages = common::session_messages(home.path(), parent);
    let text = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .and_then(|m| m["content"].as_str())
        .unwrap();
    let start = text.find('{').unwrap();
    let result: Value = serde_json::from_str(&text[start..=text.rfind('}').unwrap()]).unwrap();
    let results = result["results"].as_array().unwrap();
    assert_eq!(results[0]["exit_reason"], "budget_exhausted", "{result}");
    assert_eq!(results[1]["exit_reason"], "final_answer", "{result}");
    assert_eq!(results[1]["summary"], "quick done", "{result}");
}
