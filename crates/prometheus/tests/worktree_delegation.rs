//! Subagent worktree isolation on the mock provider (spec: git-worktrees,
//! Subagent worktree isolation; subagent-delegation, Worktree isolation
//! for children): each child works and commits in its own tree, which is
//! disposed of by what it holds when the child finishes.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{append_config, command, mock_home, ok, run, stderr};
use serde_json::Value;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn repo(home: &Path) -> PathBuf {
    let work = home.canonicalize().unwrap().join("repo");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    git(&work, &["config", "user.name", "t"]);
    git(&work, &["config", "user.email", "t@example.invalid"]);
    std::fs::write(work.join("README.md"), "hello\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    work
}

fn isolated_home() -> tempfile::TempDir {
    let home = mock_home();
    append_config(home.path(), "\n[delegation]\nworktree_isolation = true\n");
    home
}

/// The parent delegates `goal`; the child follows `child_rules`.
fn delegate(home: &Path, dir: &Path, goal: &str, child_rules: &str) -> Output {
    let script = format!(
        r#"
[[rule]]
user = "please delegate"
[[rule.tool_calls]]
name = "delegate_task"
arguments = {{ tasks = [{{ goal = "{goal}", toolsets = ["terminal"] }}] }}

{child_rules}

[[rule]]
tool_result_regex = "(?s).*"
repeat = true
text = "parent saw the result"
"#
    );
    let path = home.join("script.toml");
    std::fs::write(&path, script).unwrap();
    command(home)
        .current_dir(dir)
        .args(["--json", "-z", "please delegate", "--mock-script"])
        .arg(&path)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// The child's `SubagentResult` from the parent's transcript.
fn child_result(home: &Path) -> Value {
    let listed: Value =
        serde_json::from_str(&ok(&run(home, &["--json", "sessions", "list", "--all"]))).unwrap();
    let parent = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source"] != "subagent")
        .unwrap()
        .clone();
    let messages = common::session_messages(home, parent["id"].as_str().unwrap());
    let result = messages
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["content"].as_str())
        .find(|c| c.contains("subagent_id"))
        .expect("the delegate_task result")
        .to_owned();
    let start = result.find('{').unwrap();
    let end = result.rfind('}').unwrap();
    let body: Value = serde_json::from_str(&result[start..=end]).unwrap();
    body["results"][0].clone()
}

/// Scenario: Child commits work.
#[test]
fn a_childs_commit_lands_on_its_branch_and_its_tree_is_removed() {
    let home = isolated_home();
    let work = repo(home.path());
    let out = delegate(
        home.path(),
        &work,
        "Commit the fix",
        r#"
[[rule]]
user_regex = "Commit the fix"
tool_result_regex = "fix the bug"
text = "committed"

[[rule]]
user_regex = "Commit the fix"
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "pwd && echo fixed > fix.txt && git add fix.txt && git commit -q -m 'fix the bug' && git log --oneline -1" }
"#,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let result = child_result(home.path());
    let id = result["subagent_id"].as_str().unwrap();
    let tree = &result["worktree"];
    assert_eq!(
        tree["branch"],
        format!("prometheus/subagent-{id}"),
        "{result}"
    );
    let commits = tree["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 1, "{result}");
    assert!(
        commits[0].as_str().unwrap().ends_with("fix the bug"),
        "{result}"
    );
    assert_eq!(tree["dirty"], false, "{result}");
    assert_eq!(tree["pruned"], false, "{result}");
    assert_eq!(
        tree["archive_ref"],
        format!("refs/prometheus/archive/subagent-{id}")
    );
    assert!(!work.join(format!(".worktrees/subagent-{id}")).exists());
    // The branch stays for the parent to merge; the main checkout is
    // untouched.
    let branch = format!("prometheus/subagent-{id}");
    assert_eq!(
        git(&work, &["log", "-1", "--format=%s", &branch]),
        "fix the bug"
    );
    assert_eq!(git(&work, &["status", "--porcelain"]), "");
    assert!(!work.join("fix.txt").exists());
}

/// Scenario: Clean child pruned.
#[test]
fn a_child_that_changed_nothing_has_its_tree_and_branch_pruned() {
    let home = isolated_home();
    let work = repo(home.path());
    let out = delegate(
        home.path(),
        &work,
        "Read the docs",
        "[[rule]]\nuser_regex = \"Read the docs\"\ntext = \"nothing to change\"\n",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let result = child_result(home.path());
    let id = result["subagent_id"].as_str().unwrap();
    assert_eq!(result["worktree"]["pruned"], true, "{result}");
    assert_eq!(result["worktree"]["inspection_failed"], false, "{result}");
    assert!(!work.join(format!(".worktrees/subagent-{id}")).exists());
    assert_eq!(
        git(
            &work,
            &["branch", "--list", &format!("prometheus/subagent-{id}")]
        ),
        ""
    );
}

/// Scenario: Failed probe keeps work.
#[test]
fn a_tree_that_cannot_be_inspected_is_kept_in_place() {
    let home = isolated_home();
    let work = repo(home.path());
    let out = delegate(
        home.path(),
        &work,
        "Break the index",
        r#"
[[rule]]
user_regex = "Break the index"
tool_result_regex = "broken"
text = "done"

[[rule]]
user_regex = "Break the index"
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "printf garbage > \"$(git rev-parse --git-dir)/index\" && echo broken" }
"#,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let result = child_result(home.path());
    let id = result["subagent_id"].as_str().unwrap();
    assert_eq!(result["worktree"]["inspection_failed"], true, "{result}");
    assert_eq!(result["worktree"]["pruned"], false, "{result}");
    assert!(work.join(format!(".worktrees/subagent-{id}")).is_dir());
}

/// Scenario: Container backend.
#[test]
fn on_a_container_backend_the_child_shares_the_workspace_with_a_note() {
    let home = isolated_home();
    append_config(home.path(), "\n[terminal]\nbackend = \"container\"\n");
    let work = repo(home.path());
    let out = delegate(
        home.path(),
        &work,
        "Look around",
        "[[rule]]\nuser_regex = \"Look around\"\ntext = \"looked\"\n",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let result = child_result(home.path());
    let note = result["note"].as_str().unwrap_or_default();
    assert!(note.contains("worktree isolation skipped"), "{result}");
    assert!(note.contains("'container'"), "{result}");
    assert!(result.get("worktree").is_none(), "{result}");
    assert!(!work.join(".worktrees").exists());
}

#[test]
fn outside_git_the_child_shares_the_workspace_with_a_note() {
    let home = isolated_home();
    let dir = home.path().canonicalize().unwrap().join("plain");
    std::fs::create_dir_all(&dir).unwrap();
    let script_home = home.path();
    let script = r#"
[[rule]]
user = "please delegate"
[[rule.tool_calls]]
name = "delegate_task"
arguments = { tasks = [{ goal = "Look around" }] }

[[rule]]
user_regex = "Look around"
text = "looked"

[[rule]]
tool_result_regex = "(?s).*"
repeat = true
text = "parent saw the result"
"#;
    let path = script_home.join("script.toml");
    std::fs::write(&path, script).unwrap();
    let out = command(home.path())
        .current_dir(&dir)
        .env(
            "GIT_CEILING_DIRECTORIES",
            home.path().canonicalize().unwrap(),
        )
        .args(["--json", "-z", "please delegate", "--mock-script"])
        .arg(&path)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let result = child_result(home.path());
    assert!(
        result["note"]
            .as_str()
            .unwrap_or_default()
            .contains("not inside a git repository"),
        "{result}"
    );
}
