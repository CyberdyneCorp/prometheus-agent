//! The 60.5 journeys that drive a pseudo-terminal: a spoken edit with its
//! diagnostic, and a German TUI in an active project beside a home-channel
//! send.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use expectrl::session::OsSession;
use expectrl::{Eof, Expect};
use serde_json::Value;

use crate::common::gateway::{Daemon, gateway_home_with, sent};
use crate::common::tui::{enter, finish, opened, see};
use crate::common::{
    append_config, command, mock_home, newest_session, ok, run, session_messages, stderr,
};

/// A canonical directory `rel` under `home`.
fn dir(home: &Path, rel: &str) -> PathBuf {
    let path = home.join(rel);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::canonicalize(path).unwrap()
}
/// Spoken input in, an edit, its diagnostic, spoken answer out.
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

/// The TUI on `home` with its working directory in `cwd`.
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
