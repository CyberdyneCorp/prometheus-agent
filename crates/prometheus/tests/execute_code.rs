//! `execute_code` in real sessions (spec: subagent-delegation,
//! Programmatic tool calling under the caller's grant; `execute_code` RPC
//! channel and limits; `execute_code` pre-execution guards). Scripts run
//! on the managed Python runtime (seeded from `pmt_pybridge::testkit`;
//! needs uv on PATH) in the session's sandbox.

mod common;

use std::path::Path;
use std::process::Output;

use common::{append_config, command, mock_home, stderr};
use serde_json::Value;

fn oneshot(home: &Path, script: &str) -> Output {
    let path = home.join("script.toml");
    std::fs::write(&path, script).unwrap();
    command(home)
        .current_dir(home)
        .args(["--json", "-z", "run it", "--mock-script"])
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

/// The parent runs `code` with `execute_code`, then answers.
fn running(code: &str) -> String {
    let code = serde_json::to_string(code).unwrap();
    format!(
        r#"
[[rule]]
user = "run it"
[[rule.tool_calls]]
name = "execute_code"
arguments = {{ code = {code} }}

[[rule]]
tool_result_regex = "(?s).*"
repeat = true
text = "done"
"#
    )
}

/// The `execute_code` result the parent saw, without its provenance
/// wrapper (script output is untrusted content).
fn code_result(home: &Path) -> String {
    let text = common::active_messages(home)
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .next()
        .unwrap_or_default();
    let inner = text
        .split_once(">\n")
        .filter(|_| text.starts_with("<untrusted-content"))
        .map_or(text.as_str(), |(_, rest)| rest);
    inner
        .strip_suffix("\n</untrusted-content>")
        .unwrap_or(inner)
        .to_owned()
}

fn stdout_of(result: &str) -> String {
    let body: Value = serde_json::from_str(result).unwrap_or_else(|e| panic!("{e}: {result}"));
    body["stdout"].as_str().unwrap_or_default().to_owned()
}

/// Scenario: Empty code.
#[test]
fn whitespace_code_runs_nothing() {
    let home = mock_home();
    let out = oneshot(home.path(), &running("   \n "));
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(code_result(home.path()), "No code provided.");
}

/// Scenario: Empty intersection never widens.
#[test]
fn an_empty_tool_intersection_is_refused_before_any_code_runs() {
    let home = mock_home();
    append_config(
        home.path(),
        "[tools]\nenabled_toolsets = [\"code_execution\"]\n\n[code_execution]\nallowed_tools = [\"terminal\"]\n",
    );
    let marker = home.path().join("ran");
    let code = format!("open({:?}, 'w').write('x')", marker.to_string_lossy());
    let out = oneshot(home.path(), &running(&code));
    assert!(out.status.success(), "{}", stderr(&out));
    let result = code_result(home.path());
    assert!(
        result.contains("none of code_execution.allowed_tools is granted"),
        "{result}"
    );
    assert!(!marker.exists(), "no code ran");
}

/// Scenario: Stub limited to the grant; Secrets absent from the script
/// environment.
#[test]
fn stubs_follow_the_grant_and_the_environment_holds_no_secret() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    append_config(
        home.path(),
        "[tools]\nenabled_toolsets = [\"file\", \"code_execution\"]\n\n\
         [code_execution]\nallowed_tools = [\"read_file\", \"search_files\", \"terminal\"]\n",
    );
    common::ok(&common::run_with_stdin(
        home.path(),
        &["secrets", "set", "OPENAI_API_KEY"],
        "sk-test-secret-value\n",
    ));
    std::fs::write(home.path().join("notes.txt"), "hello from a file").unwrap();
    let code = "import os, prometheus_tools as t\n\
                print(sorted(os.environ))\n\
                print(dict(os.environ))\n\
                print(t.read_file(path='notes.txt'))\n\
                try:\n    t.call('terminal', command='echo hi')\nexcept t.ToolError as e:\n    print('refused:', e)\n";
    let out = oneshot(home.path(), &running(code));
    assert!(out.status.success(), "{}", stderr(&out));
    let result = code_result(home.path());
    let stdout = stdout_of(&result);
    assert!(stdout.contains("hello from a file"), "{result}");
    assert!(
        stdout.contains("refused: Tool 'terminal' is not available in execute_code. Available: read_file, search_files"),
        "{result}"
    );
    assert!(
        !stdout.contains("OPENAI_API_KEY") && !stdout.contains("sk-test-secret-value"),
        "{stdout}"
    );
    let body: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(
        body["tool_calls"], 1,
        "the refused stub did not use the budget"
    );
}

/// Scenario: Script sandboxed like the session.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn a_session_without_network_blocks_the_scripts_connections() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    append_config(
        home.path(),
        "[security]\ncapabilities = [\"fs.read:<workspace>/**\", \"fs.write:<workspace>/**\", \
         \"fs.read:<scratch>/**\", \"fs.write:<scratch>/**\", \"fs.read:<toolchain>\", \
         \"process.spawn:*\", \"llm.call\"]\n\n\
         [tools]\nenabled_toolsets = [\"file\", \"code_execution\"]\n",
    );
    let code = "import socket\n\
                try:\n    socket.create_connection(('1.1.1.1', 443), timeout=5)\n    print('CONNECTED')\n\
                except OSError as e:\n    print('blocked:', type(e).__name__)\n";
    let out = oneshot(home.path(), &running(code));
    assert!(out.status.success(), "{}", stderr(&out));
    let result = code_result(home.path());
    let stdout = stdout_of(&result);
    assert!(
        stdout.contains("blocked:") && !stdout.contains("CONNECTED"),
        "{result}"
    );
}
