//! Tool progress, the focus view and display preferences of the line
//! interface (spec: cli, Tool progress display and focus view; Display
//! preferences).

#![cfg(unix)]

mod common;

use std::fmt::Write as _;

use common::pty::{exit, prompt, spawn};
use common::{mock_home, ok, run};
use expectrl::Expect;

/// A mock script: each of `one` and `two` makes four `terminal` calls,
/// then answers `worked <prompt>`.
fn four_calls(home: &std::path::Path) -> String {
    let mut script = String::new();
    for word in ["one", "two"] {
        let call = format!(
            "[[rule.tool_calls]]\nname = \"terminal\"\narguments = {{ command = \"echo r-{word}\" }}\n"
        );
        let _ = write!(
            script,
            "[[rule]]\nuser = \"{word}\"\ntool_result = \"r-{word}\"\nrepeat = true\ntext = \"worked {word}\"\n\n[[rule]]\nuser = \"{word}\"\nrepeat = true\n{}\n",
            call.repeat(4)
        );
    }
    let path = home.join("work.toml");
    std::fs::write(&path, script).unwrap();
    path.to_string_lossy().into_owned()
}

/// Scenario: Focus restores previous mode.
#[test]
fn focus_hides_tool_lines_and_restores_the_previous_mode() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "display.tool_progress", "verbose"],
    ));
    let script = four_calls(home.path());
    let mut p = spawn(home.path(), &["--mock-script", &script]);
    prompt(&mut p);
    p.send_line("one").unwrap();
    p.expect("⚙ terminal {\"command\":\"echo r-one\"}").unwrap();
    p.expect("↳ terminal ok").unwrap();
    p.expect("worked one").unwrap();
    p.expect("4 tools").unwrap();
    prompt(&mut p);
    p.send_line("/focus on").unwrap();
    p.expect("focus on").unwrap();
    let badge = p.expect("› ").unwrap();
    let before = String::from_utf8_lossy(badge.before()).into_owned();
    assert!(before.contains("focus"), "{before:?}");
    p.send_line("two").unwrap();
    p.expect("4 tool lines hidden").unwrap();
    p.expect("worked two").unwrap();
    p.expect("focus › ").unwrap();
    p.send_line("/focus off").unwrap();
    p.expect("focus off: tool progress verbose").unwrap();
    prompt(&mut p);
    exit(p);
}

#[test]
fn all_accumulates_consecutive_calls_of_one_tool() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "display.tool_progress", "all"],
    ));
    let script = four_calls(home.path());
    let mut p = spawn(home.path(), &["--mock-script", &script]);
    prompt(&mut p);
    p.send_line("one").unwrap();
    p.expect("⚙ terminal ×4").unwrap();
    p.expect("worked one").unwrap();
    prompt(&mut p);
    p.send_line("/verbose").unwrap();
    p.expect("tool progress: verbose").unwrap();
    prompt(&mut p);
    exit(p);
}

/// Scenario: Bell on approval.
#[test]
fn an_approval_prompt_rings_the_bell_when_asked_to() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "display.bell_on_prompt", "true"],
    ));
    let script = home.path().join("danger.toml");
    std::fs::write(
        &script,
        "[[rule]]\nuser = \"clean\"\n[[rule.tool_calls]]\nname = \"terminal\"\narguments = { command = \"git reset --hard\" }\n\n[[rule]]\nrepeat = true\ntext = \"stopped\"\n",
    )
    .unwrap();
    let mut p = spawn(home.path(), &["--mock-script", script.to_str().unwrap()]);
    prompt(&mut p);
    p.send_line("clean").unwrap();
    p.expect("\x07").unwrap();
    p.expect("approve?").unwrap();
    p.send_line("d").unwrap();
    p.expect("stopped").unwrap();
    prompt(&mut p);
    exit(p);
}

#[test]
fn timestamps_toggle_prefixes_answers_and_persist() {
    let home = mock_home();
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("/ts status").unwrap();
    p.expect("timestamps are off").unwrap();
    prompt(&mut p);
    p.send_line("/timestamps on").unwrap();
    p.expect("timestamps on").unwrap();
    prompt(&mut p);
    p.send_line("hello").unwrap();
    p.expect(expectrl::Regex(r"\[\d\d:\d\d\] mock: hello"))
        .unwrap();
    prompt(&mut p);
    exit(p);
    let value = ok(&run(home.path(), &["config", "get", "display.timestamps"]));
    assert!(value.contains("true"), "{value}");
}
