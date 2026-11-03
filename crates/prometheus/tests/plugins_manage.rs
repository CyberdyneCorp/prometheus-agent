//! `prometheus plugins list|enable|disable|grants` and plugins in a session
//! (spec: plugin-system, Opt-in enablement; Plugin kinds and manifest;
//! Capability grants are user-approved and digest-bound; Plugin tool
//! registration; `pre_tool_call` directives).

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;

use self::common::{command, mock_home, ok, run, stderr, stdout};

fn plugin_dir(home: &Path, id: &str) -> PathBuf {
    home.join("profiles/default/plugins").join(id)
}

/// Places plugin `id` (crate `component`) with `[capabilities]` text.
fn place(home: &Path, id: &str, component: &str, world: &str, capabilities: &str) {
    let dir = plugin_dir(home, id);
    pmt_plugins::testkit::install(component, &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "manifest_version = 1\nid = \"{id}\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
             description = \"test\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
             worlds = [\"{world}@1.0\"]\n\n[capabilities]\n{capabilities}\n"
        ),
    )
    .unwrap();
}

fn line_of(listing: &str, id: &str) -> String {
    listing
        .lines()
        .find(|l| l.starts_with(&format!("{id}  ")))
        .unwrap_or_else(|| panic!("{id} not listed:\n{listing}"))
        .to_owned()
}

#[test]
fn a_discovered_plugin_stays_disabled_until_enabled_with_approval() {
    // Spec scenario "Installed is not enabled", and the approval flow.
    let home = mock_home();
    place(
        home.path(),
        "echo",
        "sample-tool",
        "tool",
        "network = [\"api.example.com\"]",
    );
    let listing = ok(&run(home.path(), &["plugins", "list"]));
    assert_eq!(
        line_of(&listing, "echo"),
        "echo  1.0.0  wasm  user  disabled"
    );

    let out = run(home.path(), &["plugins", "enable", "echo"]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("capability approval required"),
        "{}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("connect to api.example.com"),
        "{}",
        stderr(&out)
    );
    let listing = ok(&run(home.path(), &["plugins", "list"]));
    assert!(line_of(&listing, "echo").ends_with("disabled"));

    ok(&run(
        home.path(),
        &["plugins", "enable", "echo", "--grant", "all"],
    ));
    let listing = ok(&run(home.path(), &["plugins", "list"]));
    assert_eq!(line_of(&listing, "echo"), "echo  1.0.0  wasm  user  loaded");
    let shown = ok(&run(home.path(), &["plugins", "grants", "show", "echo"]));
    assert!(
        shown.contains("granted  network api.example.com"),
        "{shown}"
    );
    let grants = ok(&run(home.path(), &["grants", "list"]));
    assert!(grants.contains("net.connect:api.example.com"), "{grants}");

    ok(&run(home.path(), &["plugins", "disable", "echo"]));
    let listing = ok(&run(home.path(), &["plugins", "list"]));
    assert!(line_of(&listing, "echo").ends_with("disabled"));
}

#[test]
fn a_changed_plugin_needs_approval_again() {
    let home = mock_home();
    place(
        home.path(),
        "echo",
        "sample-tool",
        "tool",
        "secrets = [\"ECHO_KEY\"]",
    );
    ok(&run(
        home.path(),
        &["plugins", "enable", "echo", "--grant", "all"],
    ));
    std::fs::write(plugin_dir(home.path(), "echo").join("notes.md"), "edited").unwrap();
    let listing = ok(&run(home.path(), &["plugins", "list"]));
    assert!(
        line_of(&listing, "echo").contains("changed since its capabilities were approved"),
        "{listing}"
    );
    ok(&run(home.path(), &["plugins", "grants", "approve", "echo"]));
    let listing = ok(&run(home.path(), &["plugins", "list"]));
    assert!(line_of(&listing, "echo").ends_with("loaded"), "{listing}");
    ok(&run(
        home.path(),
        &["plugins", "grants", "revoke", "echo", "secrets ECHO_KEY"],
    ));
    let shown = ok(&run(home.path(), &["plugins", "grants", "show", "echo"]));
    assert!(shown.contains("pending  secrets ECHO_KEY"), "{shown}");
}

#[test]
fn manifest_errors_are_listed_and_refused() {
    // Spec scenario "Unknown capability key fails closed".
    let home = mock_home();
    place(
        home.path(),
        "bad",
        "sample-tool",
        "tool",
        "netwrok = [\"api.example.com\"]",
    );
    let out = run(home.path(), &["--json", "plugins", "list"]);
    let listing: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let row = &listing["plugins"][0];
    assert_eq!(row["id"], "bad");
    assert_eq!(row["state"], "error");
    assert_eq!(row["problem"], "unknown capability 'netwrok'");
    let out = run(home.path(), &["plugins", "enable", "bad", "--grant", "all"]);
    assert_eq!(out.status.code(), Some(65), "{}", stderr(&out));
    let out = run(home.path(), &["plugins", "enable", "missing"]);
    assert_eq!(out.status.code(), Some(66), "{}", stderr(&out));
}

#[test]
fn an_enabled_plugin_tool_and_hook_serve_a_session() {
    let home = mock_home();
    place(home.path(), "echo", "sample-tool", "tool", "");
    place(
        home.path(),
        "guard",
        "sample-hook",
        "hook",
        "hooks = [\"pre_tool_call\", \"pre_llm_call\"]",
    );
    ok(&run(home.path(), &["plugins", "enable", "echo"]));
    ok(&run(
        home.path(),
        &["plugins", "enable", "guard", "--grant", "all"],
    ));
    let script = home.path().join("script.toml");
    std::fs::write(
        &script,
        r#"
[[rule]]
user = "Answer briefly"
[[rule.tool_calls]]
name = "echo"
arguments = { text = "hi there" }
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "echo nope" }

[[rule]]
tool_result = "the sample hook blocks the terminal"
text = "echoed and blocked"
"#,
    )
    .unwrap();
    let out = command(home.path())
        .args(["-z", "go", "--mock-script", script.to_str().unwrap()])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(stdout(&out), "echoed and blocked\n", "{}", stderr(&out));
    let messages = self::common::active_messages(home.path());
    let tool_results: Vec<String> = messages
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        tool_results.iter().any(|t| t.contains("hi there")),
        "{tool_results:?}"
    );
    // The hook's context reached the model but was never stored.
    assert!(
        messages.iter().all(|m| !m["content"]
            .as_str()
            .unwrap_or_default()
            .contains("Answer briefly")),
        "{messages:?}"
    );
}

#[test]
fn a_plugin_request_to_a_silent_server_ends_with_the_call_timeout() {
    // Spec: WASM sandbox resource limits. A host call blocks outside the
    // epoch deadline, so the plugin's HTTP request carries the call bound;
    // otherwise the tool would wait for the dispatcher's own timeout.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = silent.local_addr().unwrap().port();
    let held = std::thread::spawn(move || {
        let (stream, _) = silent.accept().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(30));
        drop(stream);
    });
    let home = mock_home();
    place(
        home.path(),
        "probe",
        "testing-probe",
        "tool",
        &format!("network = [\"127.0.0.1:{port}\"]"),
    );
    ok(&run(
        home.path(),
        &["plugins", "enable", "probe", "--grant", "all"],
    ));
    for (key, value) in [
        ("plugins.wasm.call_timeout_ms", "500"),
        ("tools.default_timeout_secs", "20"),
    ] {
        ok(&run(home.path(), &["config", "set", key, value]));
    }
    let script = home.path().join("script.toml");
    std::fs::write(
        &script,
        format!(
            r#"
[[rule]]
user = "fetch"
[[rule.tool_calls]]
name = "fetch"
arguments = {{ v = "http://127.0.0.1:{port}/" }}

[[rule]]
tool_result_regex = "."
text = "done"
"#
        ),
    )
    .unwrap();
    let started = std::time::Instant::now();
    let out = command(home.path())
        .args(["-z", "fetch", "--mock-script", script.to_str().unwrap()])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let took = started.elapsed();
    assert_eq!(stdout(&out), "done\n", "{}", stderr(&out));
    assert!(
        took < std::time::Duration::from_secs(10),
        "the request outlived the call bound: {took:?}"
    );
    let results: Vec<String> = self::common::active_messages(home.path())
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        results.iter().any(|r| r.contains("request timed out")),
        "{results:?}"
    );
    drop(held);
}
