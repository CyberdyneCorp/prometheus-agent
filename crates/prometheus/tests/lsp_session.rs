//! Language-server diagnostics in a real session (spec: lsp-diagnostics,
//! Diagnostics introduced by an edit; Language servers run sandboxed): a
//! stand-in server (`python3`) run under the session's sandbox reports an
//! edit's new diagnostics; a verdict slower than the write arrives as a
//! new message before the final answer stands.

#![cfg(unix)] // the stand-in server is a python3 script

mod common;

use std::path::Path;
use std::process::Stdio;

use common::{append_config, command, mock_home, newest_session, session_messages, stderr, stdout};

/// A git workspace outside any repository, holding the stand-in server
/// (the server reads only its workspace).
fn workspace() -> tempfile::TempDir {
    let work = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(work.path().join(".git")).unwrap();
    std::fs::create_dir_all(work.path().join("tools")).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../pmt-tools/tests/lsp/fake_lsp.py");
    std::fs::copy(script, work.path().join("tools/fake_lsp.py")).unwrap();
    work
}

#[test]
fn a_slow_verdict_reaches_the_model_before_its_final_answer() {
    let home = mock_home();
    let work = workspace();
    let work_dir = std::fs::canonicalize(work.path()).unwrap();
    append_config(
        home.path(),
        &format!(
            "\n[lsp]\nwait_timeout_ms = 10000\n[lsp.servers.pyright]\ncommand = [\"python3\", \"{}\"]\nenv = {{ FAKE_LSP_DELAY_MS = \"300\" }}\n",
            work_dir.join("tools/fake_lsp.py").display()
        ),
    );
    let script = home.path().join("edit.toml");
    std::fs::write(
        &script,
        r#"
[[rule]]
user = "edit"
[[rule.tool_calls]]
name = "write_file"
arguments = { path = "app.py", content = "BAD_name()\n" }

[[rule]]
repeat = true
text = "done"
"#,
    )
    .unwrap();
    let out = command(home.path())
        .args(["-z", "edit", "--mock-script"])
        .arg(&script)
        .arg("--cwd")
        .arg(&work_dir)
        .arg("--yolo")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "done\n");
    let messages = session_messages(home.path(), &newest_session(home.path()));
    let notices: Vec<&str> = messages
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_str())
        .filter(|text| text.contains("LSP diagnostics introduced by your earlier edit of"))
        .collect();
    assert_eq!(notices.len(), 1, "{messages:#?}");
    assert!(
        notices[0].contains("ERROR [1:1] \"name\" is not defined [undefined] (pyright)"),
        "{}",
        notices[0]
    );
    assert!(notices[0].contains(&work_dir.join("app.py").display().to_string()));
}
