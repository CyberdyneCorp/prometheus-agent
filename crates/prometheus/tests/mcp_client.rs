//! MCP servers in sessions (spec: plugin-system, MCP client servers;
//! tool-system, MCP runtime safety; Tool search progressive disclosure),
//! against the reference fixture server: `mcp add|list|remove|test|status`,
//! tools reaching sessions, a crashed server, the sandboxed environment,
//! approval of untrusted tools and tool search.

mod common;

use std::path::Path;
use std::process::Stdio;

use self::common::{command, mock_home, newest_session, ok, run, session_messages, stderr};

fn fixture() -> String {
    pmt_mcp::testkit::fixture().to_string_lossy().into_owned()
}

/// `mcp add <name> --command <fixture> [args...]`.
fn add(home: &Path, name: &str, extra: &[&str]) {
    let fixture = fixture();
    let mut args = vec!["mcp", "add", name, "--command", &fixture];
    args.extend_from_slice(extra);
    ok(&run(home, &args));
}

fn script(home: &Path, body: &str) -> String {
    let path = home.join("script.toml");
    std::fs::write(&path, body).unwrap();
    path.to_string_lossy().into_owned()
}

fn one_shot(home: &Path, script: &str) -> String {
    let out = command(home)
        .args(["-z", "go", "--mock-script", script])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The text of every tool message of the newest session.
fn tool_results(home: &Path) -> Vec<String> {
    session_messages(home, &newest_session(home))
        .into_iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m.to_string())
        .collect()
}

#[test]
fn servers_are_added_listed_tested_and_removed() {
    let home = mock_home();
    // `--args` takes every word after it.
    add(
        home.path(),
        "fx",
        &["--trust", "full", "--args", "--mode", "normal"],
    );
    let listed = ok(&run(home.path(), &["mcp", "list"]));
    assert!(listed.starts_with("fx  stdio  "), "{listed}");
    assert!(listed.contains("--mode normal  trust full"), "{listed}");
    let again = run(home.path(), &["mcp", "add", "fx", "--url", "http://x"]);
    assert_eq!(again.status.code(), Some(65), "{}", stderr(&again));
    let tested = ok(&run(home.path(), &["mcp", "test", "fx"]));
    assert!(tested.contains(": 9 tools (9 registered)"), "{tested}");
    add(
        home.path(),
        "broken",
        &["--args", "--mode", "crash-on-start"],
    );
    let failed = run(home.path(), &["mcp", "test", "broken"]);
    assert_eq!(failed.status.code(), Some(69), "{}", stderr(&failed));
    assert!(
        stderr(&failed).contains("the server process exited"),
        "{}",
        stderr(&failed)
    );
    let status = ok(&run(home.path(), &["--json", "mcp", "status"]));
    let status: serde_json::Value = serde_json::from_str(&status).unwrap();
    assert_eq!(status["servers"][0]["name"], "broken");
    assert_eq!(status["servers"][0]["state"], "unavailable");
    assert_eq!(status["servers"][1]["state"], "connected");
    assert_eq!(status["servers"][1]["tools"], 9);
    // `enabled = false` keeps a server configured without connecting it.
    ok(&run(
        home.path(),
        &["config", "set", "mcp.servers.broken.enabled", "false"],
    ));
    let status = ok(&run(home.path(), &["mcp", "status"]));
    assert!(
        status.contains("broken  stdio  disabled  0 tools"),
        "{status}"
    );
    ok(&run(home.path(), &["mcp", "rm", "broken"]));
    let missing = run(home.path(), &["mcp", "remove", "broken"]);
    assert_eq!(missing.status.code(), Some(66));
    assert!(!ok(&run(home.path(), &["mcp", "list"])).contains("broken"));
}

#[test]
fn a_configured_servers_tools_reach_a_session() {
    let home = mock_home();
    add(home.path(), "fx", &["--trust", "full"]);
    let script = script(
        home.path(),
        r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "mcp__fx__echo"
arguments = { text = "hello from mcp" }

[[rule]]
tool_result = "hello from mcp"
text = "echoed"
"#,
    );
    assert_eq!(one_shot(home.path(), &script), "echoed\n");
}

#[test]
fn crashed_mcp_server_does_not_break_the_session() {
    let home = mock_home();
    add(home.path(), "github", &["--trust", "full"]);
    let script = script(
        home.path(),
        r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "mcp__github__crash"
arguments = {}

[[rule]]
tool_result = "MCP server 'github' unavailable"
text = "carried on"
"#,
    );
    assert_eq!(one_shot(home.path(), &script), "carried on\n");
}

#[test]
fn provider_key_not_forwarded() {
    let home = mock_home();
    common::run_with_stdin(
        home.path(),
        &["secrets", "set", "OPENAI_API_KEY"],
        "sk-test-0123456789abcdefghij",
    );
    common::run_with_stdin(
        home.path(),
        &["secrets", "set", "GH_TOKEN"],
        "gh-token-value",
    );
    add(
        home.path(),
        "fx",
        &[
            "--trust",
            "full",
            "--env",
            "MODE=test",
            "--env-secret",
            "GH_TOKEN",
        ],
    );
    let script = script(
        home.path(),
        r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "mcp__fx__env"
arguments = {}

[[rule]]
tool_result = "PATH"
text = "listed"
"#,
    );
    assert_eq!(one_shot(home.path(), &script), "listed\n");
    let results = tool_results(home.path()).join("\n");
    assert!(results.contains("GH_TOKEN"), "{results}");
    assert!(results.contains("MODE"), "{results}");
    assert!(!results.contains("OPENAI_API_KEY"), "{results}");
    assert!(!results.contains("PROMETHEUS_HOME"), "{results}");
}

#[test]
fn an_untrusted_servers_writing_tool_needs_approval_its_read_only_tool_does_not() {
    let home = mock_home();
    // The sandbox lets the server write only where its grant says.
    let logs = home.path().join("mcp-logs");
    std::fs::create_dir_all(&logs).unwrap();
    let log = logs.join("calls.log");
    add(
        home.path(),
        "fx",
        &["--args", "--log", log.to_str().unwrap()],
    );
    let grant = format!("[{:?}]", format!("{}/**", logs.display()));
    ok(&run(
        home.path(),
        &[
            "config",
            "set",
            "mcp.servers.fx.capabilities.fs_write",
            &grant,
        ],
    ));
    let calling = |tool: &str, arguments: &str| {
        script(
            home.path(),
            &format!(
                "[[rule]]\nuser = \"go\"\n[[rule.tool_calls]]\nname = \"{tool}\"\narguments = {arguments}\n\n\
                 [[rule]]\ntool_result = \"\"\ntext = \"done\"\n"
            ),
        )
    };
    // A one-shot run cannot ask: `approvals.single_query_mode` (deny)
    // decides, the server is never called and the run ends with exit 4.
    let write = calling("mcp__fx__create_issue", "{ title = \"bug\" }");
    let out = command(home.path())
        .args(["-z", "go", "--mock-script", &write])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert!(stderr(&out).contains("a tool call needed an approval"));
    let read = calling("mcp__fx__echo", "{ text = \"read only ran\" }");
    assert_eq!(one_shot(home.path(), &read), "done\n");
    let calls = std::fs::read_to_string(&log).unwrap();
    assert_eq!(calls, "call echo\n");
}

#[test]
fn with_tool_search_only_the_bridge_tools_are_offered_and_tool_call_runs_the_server_tool() {
    let home = mock_home();
    add(home.path(), "fx", &["--trust", "full"]);
    ok(&run(
        home.path(),
        &["config", "set", "tools.tool_search.mode", "on"],
    ));
    let script = script(
        home.path(),
        r#"
[[rule]]
tools = "\"name\":\"mcp__fx__echo\""
text = "the server's tools were offered directly"

[[rule]]
user = "go"
tools = "\"name\":\"tool_call\""
[[rule.tool_calls]]
name = "tool_call"
arguments = { calls = [{ name = "mcp__fx__echo", arguments = { text = "via the bridge" } }] }

[[rule]]
tool_result = "via the bridge"
text = "bridged"
"#,
    );
    assert_eq!(one_shot(home.path(), &script), "bridged\n");
}

#[test]
fn safe_mode_starts_no_mcp_server() {
    let home = mock_home();
    add(home.path(), "fx", &["--trust", "full"]);
    let script = script(
        home.path(),
        r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "mcp__fx__echo"
arguments = { text = "x" }

[[rule]]
tool_result = "unknown tool: mcp__fx__echo"
text = "not there"
"#,
    );
    let out = command(home.path())
        .args([
            "--set",
            "plugins.safe_mode=true",
            "-z",
            "go",
            "--mock-script",
            &script,
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "not there\n",
        "{}",
        stderr(&out)
    );
    let status = ok(&run(
        home.path(),
        &["--set", "plugins.safe_mode=true", "mcp", "status"],
    ));
    assert!(
        status.contains("fx  stdio  skipped (safe mode)"),
        "{status}"
    );
}

#[cfg(unix)]
mod line {
    use std::path::Path;

    use expectrl::Expect as _;

    use super::common::pty::{exit, prompt, spawn};
    use super::common::{ok, run};

    /// The tools `tools list --session` reports exposed for session `id`.
    fn exposed(home: &Path, id: &str) -> Vec<String> {
        ok(&run(home, &["tools", "list", "--session", id]))
            .lines()
            .filter(|l| l.contains(" exposed"))
            .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
            .collect()
    }

    #[test]
    fn mcp_reload_brings_new_tools_next_session_unless_now() {
        let home = super::mock_home();
        let mut p = spawn(home.path(), &[]);
        prompt(&mut p);
        p.send_line("first").unwrap();
        p.expect("mock: first").unwrap();
        prompt(&mut p);
        // A server added while the session runs.
        super::add(home.path(), "fx", &["--trust", "full"]);
        p.send_line("/mcp reload").unwrap();
        p.expect("fx (stdio) connected, 9 tools").unwrap();
        p.expect("tools: next session").unwrap();
        prompt(&mut p);
        let id = super::newest_session(home.path());
        assert!(!exposed(home.path(), &id).contains(&"mcp__fx__echo".to_owned()));
        p.send_line("/mcp reload --now").unwrap();
        p.expect("tools: now (this session").unwrap();
        prompt(&mut p);
        let tools = exposed(home.path(), &id);
        assert!(tools.contains(&"mcp__fx__echo".to_owned()), "{tools:?}");
        p.send_line("/mcp").unwrap();
        p.expect("fx (stdio) connected, 9 tools").unwrap();
        prompt(&mut p);
        exit(p);
    }
}
