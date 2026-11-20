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

/// Scenario: Undo a sed edit (spec: execution-environments, Terminal
/// workspace checkpoints): with `files.snapshot.terminal = "workspace"`,
/// `/undo` also reverts what a terminal command changed in the workspace.
#[test]
fn undo_reverts_a_sed_edit_made_by_a_terminal_command() {
    let home = mock_home();
    append_config(
        home.path(),
        "\n[cli]\nconfirm_destructive = false\n\n[files.snapshot]\nterminal = \"workspace\"\n",
    );
    let work = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    std::fs::write(work.path().join("notes.txt"), "alpha\n").unwrap();
    std::fs::write(work.path().join("other.txt"), "untouched\n").unwrap();
    // BSD sed takes the backup suffix as its own argument.
    let sed = if cfg!(target_os = "macos") {
        "sed -i '' s/a/b/ notes.txt && echo made > created.txt"
    } else {
        "sed -i s/a/b/ notes.txt && echo made > created.txt"
    };
    let script = home.path().join("sed.toml");
    std::fs::write(
        &script,
        format!(
            r#"
[[rule]]
user = "edit"
[[rule.tool_calls]]
name = "terminal"
arguments = {{ command = "{sed}" }}

[[rule]]
repeat = true
text = "edited by sed"
"#
        ),
    )
    .unwrap();
    let cwd = work.path().to_str().unwrap();
    let mut p = spawn(
        home.path(),
        &["--mock-script", script.to_str().unwrap(), "--cwd", cwd],
    );
    prompt(&mut p);
    p.send_line("edit").unwrap();
    p.expect("edited by sed").unwrap();
    prompt(&mut p);
    let read = |name: &str| std::fs::read_to_string(work.path().join(name)).ok();
    assert_eq!(read("notes.txt").as_deref(), Some("blpha\n"));
    assert_eq!(read("created.txt").as_deref(), Some("made\n"));
    p.send_line("/undo").unwrap();
    p.expect("undid the last exchange").unwrap();
    prompt(&mut p);
    assert_eq!(read("notes.txt").as_deref(), Some("alpha\n"));
    assert_eq!(read("created.txt"), None);
    assert_eq!(read("other.txt").as_deref(), Some("untouched\n"));
    exit(p);
}

/// Scenario: Terminal workspace checkpoints, for a background command:
/// the file it writes after its call returned is journaled when it exits,
/// so `/undo` reverts it too.
#[test]
fn undo_reverts_a_file_a_background_command_wrote_after_it_finished() {
    let home = mock_home();
    append_config(
        home.path(),
        "\n[cli]\nconfirm_destructive = false\n\n[files.snapshot]\nterminal = \"workspace\"\n",
    );
    let work = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    std::fs::write(work.path().join("notes.txt"), "alpha\n").unwrap();
    let script = home.path().join("bg.toml");
    std::fs::write(
        &script,
        r#"
[[rule]]
user = "edit"
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "while [ ! -e go ]; do sleep 0.1; done; printf 'late\n' > notes.txt; echo made > later.txt", background = true }

[[rule]]
repeat = true
text = "started it"
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
    p.expect("started it").unwrap();
    prompt(&mut p);
    let read = |name: &str| std::fs::read_to_string(work.path().join(name)).ok();
    // The command's call returned before it wrote anything; it writes
    // once told to.
    assert_eq!(read("notes.txt").as_deref(), Some("alpha\n"));
    std::fs::write(work.path().join("go"), "").unwrap();
    let journals = home.path().join("profiles/default/snapshots/journals");
    let journaled = || {
        std::fs::read_dir(&journals)
            .into_iter()
            .flatten()
            .flatten()
            .any(|f| std::fs::read_to_string(f.path()).is_ok_and(|text| text.contains("later.txt")))
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !journaled() {
        assert!(
            std::time::Instant::now() < deadline,
            "the background command's changes were never journaled"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(read("notes.txt").as_deref(), Some("late\n"));
    p.send_line("/undo").unwrap();
    p.expect("undid the last exchange").unwrap();
    prompt(&mut p);
    assert_eq!(read("notes.txt").as_deref(), Some("alpha\n"));
    assert_eq!(read("later.txt"), None);
    exit(p);
}
