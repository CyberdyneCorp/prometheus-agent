//! `/undo` puts back the files the last turn changed (spec:
//! execution-environments, File snapshots and undo).

#![cfg(unix)]

mod common;

use common::pty::{exit, prompt, spawn};
use common::{append_config, mock_home};
use expectrl::Expect;

/// Scenario: Undo last turn (through the line interface).
#[test]
fn undo_restores_the_patched_file_and_removes_the_created_one() {
    let home = mock_home();
    append_config(home.path(), "\n[cli]\nconfirm_destructive = false\n");
    let work = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    std::fs::write(work.path().join("notes.txt"), "old line\n").unwrap();
    let script = home.path().join("edit.toml");
    std::fs::write(
        &script,
        r#"
[[rule]]
user = "edit"
[[rule.tool_calls]]
name = "patch"
arguments = { path = "notes.txt", old_string = "old", new_string = "new" }
[[rule.tool_calls]]
name = "write_file"
arguments = { path = "created.txt", content = "made\n" }

[[rule]]
repeat = true
text = "edited both"
"#,
    )
    .unwrap();
    let cwd = work.path().to_str().unwrap();
    let mut p = spawn(
        home.path(),
        &["--mock-script", script.to_str().unwrap(), "--cwd", cwd],
    );
    prompt(&mut p);
    p.send_line("edit").unwrap();
    p.expect("edited both").unwrap();
    prompt(&mut p);
    let read = |name: &str| std::fs::read_to_string(work.path().join(name)).ok();
    assert_eq!(read("notes.txt").as_deref(), Some("new line\n"));
    assert_eq!(read("created.txt").as_deref(), Some("made\n"));
    p.send_line("/undo").unwrap();
    p.expect("undid the last exchange").unwrap();
    prompt(&mut p);
    assert_eq!(read("notes.txt").as_deref(), Some("old line\n"));
    assert_eq!(read("created.txt"), None);
    exit(p);
}
