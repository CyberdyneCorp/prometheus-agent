//! Harness hooks and manual edits on the mock provider (spec:
//! harness-evolution, Harness execution confinement; Rollback and manual
//! edits).

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;

use common::{command, mock_home, newest_session, ok, run, session_messages, stderr, stdout};

const TERMINAL_RULES: &str = r#"
[[rule]]
user = "run it"
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "echo from-the-tool" }

[[rule]]
tool_result = "blocked by the harness"
text = "the harness blocked it"

[[rule]]
tool_result = "from-the-tool"
text = "the tool ran"
"#;

fn run_terminal_turn(home: &Path) -> String {
    let rules = home.join("rules.toml");
    std::fs::write(&rules, TERMINAL_RULES).unwrap();
    let work = home.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let out = command(home)
        .args([
            "-z",
            "run it",
            "--yolo",
            "--mock-script",
            rules.to_str().unwrap(),
        ])
        .current_dir(work)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    stdout(&out)
}

/// A harness whose `pre_tool_call` hook blocks the terminal tool.
fn guarded_tree(home: &Path) -> PathBuf {
    let dir = home.join("guarded");
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(
        dir.join("scripts/guard.sh"),
        "cat > /dev/null\necho 'blocked by the harness' >&2\nexit 2\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("hooks.toml"),
        "[[hooks]]\nevent = \"pre_tool_call\"\ncommand = \"sh scripts/guard.sh\"\nmatcher = \"^terminal$\"\n",
    )
    .unwrap();
    dir
}

#[test]
fn a_harness_hook_runs_sandboxed_on_its_event_and_stops_with_a_rollback() {
    let home = mock_home();
    assert_eq!(run_terminal_turn(home.path()), "the tool ran\n");
    let dir = guarded_tree(home.path());
    ok(&run(
        home.path(),
        &["harness", "apply", dir.to_str().unwrap()],
    ));
    assert_eq!(run_terminal_turn(home.path()), "the harness blocked it\n");
    let tool_rows: Vec<String> = session_messages(home.path(), &newest_session(home.path()))
        .into_iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(tool_rows, ["blocked by the harness"]);
    ok(&run(home.path(), &["harness", "rollback", "h1"]));
    assert_eq!(run_terminal_turn(home.path()), "the tool ran\n");
}

#[test]
fn harness_edit_writes_a_manual_version_from_the_editor() {
    let home = mock_home();
    let editor = home.path().join("editor.sh");
    std::fs::write(&editor, "#!/bin/sh\nprintf 'Be terse.' > \"$1\"\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out = command(home.path())
        .args(["harness", "edit", "SYSTEM.md"])
        .env("EDITOR", &editor)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("(manual of h1-"), "{}", stdout(&out));
    let shown = ok(&run(home.path(), &["harness", "show", "h2", "SYSTEM.md"]));
    assert_eq!(shown, "Be terse.");
    // An editor that changes nothing writes no version.
    let noop = home.path().join("noop.sh");
    std::fs::write(&noop, "#!/bin/sh\nexit 0\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&noop, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out = command(home.path())
        .args(["harness", "edit"])
        .env("EDITOR", &noop)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(stdout(&out).contains("no change"), "{}", stdout(&out));
}
