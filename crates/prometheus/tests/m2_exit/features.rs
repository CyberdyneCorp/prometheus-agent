//! Task 60.5: the M2 feature surfaces through the binary (spec:
//! voice-mode; web-and-media-tools, Command-template speech providers;
//! lsp-diagnostics; connectors; local-models; projects; agent-interop,
//! One-shot send command; i18n): a spoken request whose edit draws a
//! language-server diagnostic and whose answer is spoken back; a connector
//! tool found through tool search and called; a stub llama.cpp server
//! answering with the network gone; and a German TUI working in an active
//! project of a daemon that also sends to the platform's home channel.

use std::process::Stdio;

use serde_json::json;

use crate::common::{command, mock_home, ok, run, run_with_stdin, stderr, stdout, temp_home};

// The voice and TUI journeys drive a pseudo-terminal (expectrl), which
// only the unix test harness has; the rest runs on every OS.
#[cfg(unix)]
mod terminal;

#[test]
fn a_connector_tool_found_through_tool_search_is_called() {
    use pmt_tools::connectors::fake::{Fake, tool};

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let fake = runtime.block_on(Fake::start(
        &["gmail"],
        vec![tool(
            "gmail",
            "GMAIL_SEND_EMAIL",
            "Send an email from Gmail.",
        )],
    ));
    fake.add_account("gmail", Some("work"), "me@example.com", "active");
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "connectors.gateway_url", &fake.url()],
    ));
    let meta = r#"{"provider":"nous","label":"default","source":"device","obtained_at":"2026-01-01T00:00:00Z","status":"ok"}"#;
    for (name, value) in [
        ("OAUTH_NOUS_DEFAULT_ACCESS", "nous-token"),
        ("OAUTH_NOUS_DEFAULT_META", meta),
    ] {
        ok(&run_with_stdin(
            home.path(),
            &["secrets", "set", name],
            &format!("{value}\n"),
        ));
    }
    let script = home.path().join("script.toml");
    std::fs::write(
        &script,
        r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "tool_search"
arguments = { queries = ["send an email"] }

[[rule]]
tool_result = "connectors__gmail__send_email"
[[rule.tool_calls]]
name = "tool_call"
arguments = { calls = [{ name = "connectors__gmail__send_email", arguments = { to = "a@example.com" } }] }

[[rule]]
tool_result = "GMAIL_SEND_EMAIL"
text = "sent through the connector"
"#,
    )
    .unwrap();
    let out = command(home.path())
        .args(["-z", "go", "--mock-script", script.to_str().unwrap()])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "sent through the connector\n");
    let executed = fake.executed();
    assert_eq!(executed.len(), 1, "{executed:?}");
    assert_eq!(executed[0]["tool"], "GMAIL_SEND_EMAIL");
    assert_eq!(executed[0]["arguments"]["to"], "a@example.com");
}

/// A home on `llamacpp/qwen-x` with the stub llama-server installed as
/// the engine and `qwen-x` in the model library.
fn local_home() -> tempfile::TempDir {
    let home = temp_home();
    let h = home.path();
    ok(&run(h, &["config", "set", "model.provider", "llamacpp"]));
    ok(&run(h, &["config", "set", "model.default", "qwen-x"]));
    let version = pmt_local::engine::EngineIndex::embedded().version;
    let engine = h.join(format!("runtimes/llamacpp/{version}-cpu"));
    std::fs::create_dir_all(&engine).unwrap();
    let record = json!({
        "version": version, "backend": "cpu", "platform": "test",
        "exe": pmt_local::testkit::stub(), "library_dirs": [], "archives": [], "installed_at": "",
    });
    std::fs::write(
        engine.join(pmt_local::engine::RECORD_FILE),
        record.to_string(),
    )
    .unwrap();
    let model = h.join("models/qwen-x");
    std::fs::create_dir_all(&model).unwrap();
    std::fs::write(
        model.join("w.gguf"),
        pmt_local::gguf::fixture("qwen3", 65_536, 4, 2, 64),
    )
    .unwrap();
    let record = json!({
        "id": "qwen-x", "source": "catalog", "context": 65536, "added_at": "",
        "files": [{"path": model.join("w.gguf"), "role": "weights", "size": 4}],
    });
    std::fs::write(model.join("model.json"), record.to_string()).unwrap();
    std::fs::write(h.join("daemon.toml"), "[local_runtime]\nenabled = true\n").unwrap();
    home
}

#[test]
fn a_stub_local_model_answers_with_the_network_gone() {
    let home = local_home();
    // Every proxy variable names a dead proxy with no exemption: only
    // the loopback server can be reached.
    let out = command(home.path())
        .args(["chat", "-z", "hello offline"])
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "stub answer from qwen-x");
}
