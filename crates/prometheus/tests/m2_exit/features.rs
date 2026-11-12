//! Task 60.5: the M2 feature surfaces through the binary (spec:
//! voice-mode; web-and-media-tools, Command-template speech providers;
//! lsp-diagnostics; connectors; local-models; projects; agent-interop,
//! One-shot send command; i18n): a spoken request whose edit draws a
//! language-server diagnostic and whose answer is spoken back; a connector
//! tool found through tool search and called; a stub llama.cpp server
//! answering with the network gone; and a German TUI working in an active
//! project of a daemon that also sends to the platform's home channel.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use expectrl::session::OsSession;
use expectrl::{Eof, Expect};
use serde_json::{Value, json};

use crate::common::gateway::{Daemon, gateway_home_with, sent};
use crate::common::tui::{enter, finish, opened, see};
use crate::common::{
    append_config, command, mock_home, newest_session, ok, run, run_with_stdin, session_messages,
    stderr, stdout, temp_home,
};

/// A canonical directory `rel` under `home`.
fn dir(home: &Path, rel: &str) -> PathBuf {
    let path = home.join(rel);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::canonicalize(path).unwrap()
}

/// Spoken input in, an edit, its diagnostic, spoken answer out.
#[cfg(unix)]
#[test]
fn a_spoken_edit_gets_its_diagnostic_and_the_answer_is_spoken() {
    let home = mock_home();
    let work = dir(home.path(), "work");
    std::fs::create_dir_all(work.join(".git")).unwrap();
    let server = Path::new(env!("CARGO_MANIFEST_DIR")).join("../pmt-tools/tests/lsp/fake_lsp.py");
    std::fs::create_dir_all(work.join("tools")).unwrap();
    std::fs::copy(server, work.join("tools/fake_lsp.py")).unwrap();
    std::fs::write(
        home.path().join("profiles/default/script.toml"),
        r#"
[[rule]]
user = "fix the app"
[[rule.tool_calls]]
name = "write_file"
arguments = { path = "app.py", content = "BAD_name()\n" }

[[rule]]
tool_result_regex = "is not defined"
text = "I wrote app.py and the checker says name is not defined."

[[rule]]
tool_result_regex = "app.py"
text = "I wrote app.py."

[[rule]]
user_regex = "LSP diagnostics introduced"
text = "I wrote app.py and the checker says name is not defined."
"#,
    )
    .unwrap();
    append_config(
        home.path(),
        &format!(
            "\n[providers.mock]\nscript = \"script.toml\"\n\n\
             [stt]\nprovider = \"fake\"\n\n[stt.providers.fake]\ntype = \"command\"\n\
             command = \"printf 'fix the app'\"\n\n[tts]\nprovider = \"fake\"\n\n\
             [tts.providers.fake]\ntype = \"command\"\ncommand = \"cp {{input_path}} {{output_path}}\"\n\n\
             [lsp]\nwait_timeout_ms = 10000\n[lsp.servers.pyright]\n\
             command = [\"python3\", \"{}\"]\n",
            work.join("tools/fake_lsp.py").display()
        ),
    );
    // A microphone and one recorded question (the debug-build audio seam).
    let audio = dir(home.path(), "audio");
    std::fs::write(audio.join("device"), "Desk Microphone").unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../pmt-voice/tests/fixtures/question.wav"),
        audio.join("mic-00.wav"),
    )
    .unwrap();
    let mut cmd: Command = command(home.path());
    cmd.args(["--set", "ui.interface=line", "--yolo"])
        .current_dir(&work)
        .env("TERM", "xterm")
        .env("PMT_TEST_AUDIO_DIR", &audio);
    let mut p = OsSession::spawn(cmd).unwrap();
    p.set_expect_timeout(Some(Duration::from_mins(1)));
    p.expect("› ").unwrap();
    p.send_line("/voice on").unwrap();
    p.expect("Voice mode on").unwrap();
    p.expect("› ").unwrap();
    p.send_line("/voice tts").unwrap();
    p.expect("spoken replies: on").unwrap();
    p.expect("› ").unwrap();
    // Ctrl+B records; the fixture is transcribed as the prompt.
    p.send("\x02").unwrap();
    p.expect("› fix the app").unwrap();
    p.expect("the checker says name is not defined").unwrap();
    p.expect("Listening stopped").unwrap();
    p.expect("› ").unwrap();
    p.send_line("/exit").unwrap();
    p.expect(Eof).unwrap();
    // The diagnostic reached the model, in the tool result or as a late
    // notice.
    let messages = session_messages(home.path(), &newest_session(home.path()));
    let diagnostic = "is not defined [undefined] (pyright)";
    assert!(
        messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .any(|c| c.contains(diagnostic)),
        "{messages:#?}"
    );
    let played = std::fs::read_to_string(audio.join("played.log")).unwrap();
    assert!(
        played.contains("I wrote app.py and the checker says name is not defined."),
        "{played}"
    );
}

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

/// The TUI on `home` with its working directory in `cwd`.
#[cfg(unix)]
fn tui_in(home: &Path, cwd: &Path) -> OsSession {
    let mut cmd: Command = command(home);
    cmd.current_dir(cwd)
        .env("TERM", "xterm-256color")
        .env_remove("COLORFGBG")
        .env_remove("NO_COLOR");
    let mut session = OsSession::spawn(cmd).unwrap();
    session.get_process_mut().set_window_size(100, 30).unwrap();
    session.set_expect_timeout(Some(Duration::from_secs(30)));
    session
}

#[cfg(unix)]
#[test]
fn a_german_tui_in_the_active_project_and_a_home_channel_send_share_one_daemon() {
    let home = gateway_home_with(
        "[[rule]]\nrepeat = true\ntext = \"erledigt\"\n",
        "home_channel = { chat_id = \"ops\" }\n",
    );
    let home = home.path();
    ok(&run(home, &["config", "set", "display.language", "de"]));
    let code = dir(home, "src/prometheus");
    let docs = dir(home, "src/docs");
    let slug = ok(&run(
        home,
        &[
            "projects",
            "create",
            "Prometheus Agent",
            code.to_str().unwrap(),
            docs.to_str().unwrap(),
            "--use",
        ],
    ));
    assert_eq!(slug.trim(), "prometheus-agent");
    let project: Value = serde_json::from_str(&ok(&run(
        home,
        &["--json", "projects", "show", "prometheus-agent"],
    )))
    .unwrap();
    assert_eq!(project["project"]["active"], true, "{project}");
    let daemon = Daemon::start(home);

    // The TUI speaks German and works in the project's folder.
    let mut tui = tui_in(home, &code);
    opened(&mut tui);
    see(&mut tui, "Werkzeuge (");
    enter(&mut tui, "hallo");
    see(&mut tui, "erledigt");
    enter(&mut tui, "/quit");
    assert_eq!(finish(tui).1, Some(0));
    let sessions: Value = serde_json::from_str(&ok(&run(
        home,
        &[
            "--json",
            "sessions",
            "list",
            "--all",
            "--project",
            "prometheus-agent",
        ],
    )))
    .unwrap();
    assert_eq!(
        sessions["sessions"].as_array().unwrap().len(),
        1,
        "{sessions}"
    );

    // A platform-only send goes to the configured home channel.
    let out = run(home, &["gateway", "send", "--to", "test", "nightly report"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let to_ops: Vec<Value> = sent(home, "default")
        .into_iter()
        .filter(|m| m["kind"] == "send" && m["chat"] == "ops")
        .collect();
    assert_eq!(to_ops.len(), 1, "{to_ops:?}");
    assert_eq!(to_ops[0]["text"], "nightly report");
    assert_eq!(daemon.stop(home), Some(0));
}
