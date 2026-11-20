//! Fan-in join modes of `delegate_task` in a real session on the mock
//! provider (spec: subagent-delegation, Fan-in join modes).

mod common;

use std::time::{Duration, Instant};

use common::{append_config, command, mock_home, stderr, stdout};
use serde_json::Value;

/// Scenario: First success cancels the rest.
#[test]
fn first_success_returns_the_quick_child_and_interrupts_the_slow_ones() {
    let home = mock_home();
    // A one-shot run may spawn two children by default.
    append_config(home.path(), "[delegation]\noneshot_max_children = 3\n");
    let script = r#"
[[rule]]
user = "please delegate"
[[rule.tool_calls]]
name = "delegate_task"
arguments = { join = "first_success", tasks = [{ goal = "Survey slowly" }, { goal = "Answer quickly" }, { goal = "Audit slowly" }] }

[[rule]]
user_regex = "slowly"
latency_ms = 60000
text = "slow child done"

[[rule]]
user = "Answer quickly"
text = "quick child done"

[[rule]]
tool_result = ["\"join_satisfied\":true", "quick child done", "\"status\":\"interrupted\""]
text = "joined on the first success"

[[rule]]
tool_result_regex = "(?s).*"
text = "unexpected result"
"#;
    let path = home.path().join("script.toml");
    std::fs::write(&path, script).unwrap();
    let started = Instant::now();
    let out = command(home.path())
        .current_dir(home.path())
        .args(["--json", "-z", "please delegate", "--mock-script"])
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the call waited for the slow children"
    );
    let body: Value = serde_json::from_str(stdout(&out).trim()).unwrap();
    let messages = common::session_messages(home.path(), body["session_id"].as_str().unwrap());
    assert_eq!(
        body["answer"], "joined on the first success",
        "{messages:#?}"
    );
}
