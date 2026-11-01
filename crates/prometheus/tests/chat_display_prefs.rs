//! Display preferences that shape the line interface (spec: cli, Display
//! preferences): `display.final_response_markdown`, `display.inline_diffs`,
//! `display.vim_mode` and `display.compact`.

#![cfg(unix)]

mod common;

use std::time::Duration;

use common::pty::{exit, prompt, spawn};
use common::{mock_home, ok, run};
use expectrl::Expect;

/// A mock script whose every answer is `answer` (TOML basic-string body).
fn answering(home: &std::path::Path, answer: &str) -> String {
    let path = home.join("answer.toml");
    std::fs::write(
        &path,
        format!("[[rule]]\nrepeat = true\ntext = \"{answer}\"\n"),
    )
    .unwrap();
    path.to_string_lossy().into_owned()
}

/// What the terminal received up to the next idle prompt.
fn until_prompt(p: &mut expectrl::session::OsSession) -> String {
    let found = p.expect("› ").unwrap();
    String::from_utf8_lossy(found.before()).into_owned()
}

#[test]
fn the_final_answer_is_rendered_markdown_by_default() {
    let home = mock_home();
    let script = answering(home.path(), "# Plan\\n\\n- **one**\\n- two");
    let mut p = spawn(home.path(), &["--mock-script", &script]);
    prompt(&mut p);
    p.send_line("go").unwrap();
    p.expect("\x1b[1;4mPlan\x1b[22;24m").unwrap();
    p.expect("• \x1b[1mone\x1b[22m").unwrap();
    p.expect("• two").unwrap();
    prompt(&mut p);
    exit(p);
}

#[test]
fn raw_markdown_prints_the_answer_as_written() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "display.final_response_markdown", "raw"],
    ));
    let script = answering(home.path(), "# Plan\\n\\n- **one**\\n- two");
    let mut p = spawn(home.path(), &["--mock-script", &script]);
    prompt(&mut p);
    p.send_line("go").unwrap();
    p.expect("# Plan").unwrap();
    p.expect("- **one**").unwrap();
    p.expect("- two").unwrap();
    prompt(&mut p);
    exit(p);
}

/// A home with `notes.txt` and a script whose `fix` patches it.
fn patching(home: &std::path::Path) -> String {
    std::fs::write(home.join("notes.txt"), "alpha\nbeta\n").unwrap();
    let path = home.join("patch.toml");
    std::fs::write(
        &path,
        "[[rule]]\nuser = \"fix\"\n[[rule.tool_calls]]\nname = \"patch\"\narguments = { path = \"notes.txt\", old_string = \"beta\", new_string = \"gamma\" }\n\n[[rule]]\nrepeat = true\ntext = \"fixed\"\n",
    )
    .unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn a_file_edit_shows_its_diff_inline() {
    let home = mock_home();
    let script = patching(home.path());
    let mut p = spawn(home.path(), &["--mock-script", &script]);
    prompt(&mut p);
    p.send_line("fix").unwrap();
    p.expect("⚙ patch").unwrap();
    p.expect("@@ -1,2 +1,2 @@").unwrap();
    p.expect("\x1b[31m    -beta").unwrap();
    p.expect("\x1b[32m    +gamma").unwrap();
    p.expect("fixed").unwrap();
    prompt(&mut p);
    exit(p);
    let edited = std::fs::read_to_string(home.path().join("notes.txt")).unwrap();
    assert_eq!(edited, "alpha\ngamma\n");
}

#[test]
fn inline_diffs_off_shows_only_the_tool_line() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "display.inline_diffs", "false"],
    ));
    let script = patching(home.path());
    let mut p = spawn(home.path(), &["--mock-script", &script]);
    prompt(&mut p);
    p.send_line("fix").unwrap();
    let shown = p.expect("fixed").unwrap();
    let before = String::from_utf8_lossy(shown.before()).into_owned();
    assert!(before.contains("⚙ patch"), "{before:?}");
    assert!(!before.contains("+gamma"), "{before:?}");
    prompt(&mut p);
    exit(p);
    let edited = std::fs::read_to_string(home.path().join("notes.txt")).unwrap();
    assert_eq!(edited, "alpha\ngamma\n");
}

#[test]
fn vim_mode_edits_the_line_with_vi_keys() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "display.vim_mode", "true"],
    ));
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send("xx hello").unwrap();
    // Esc alone (not the start of an escape sequence): normal mode.
    std::thread::sleep(Duration::from_millis(600));
    p.send("\x1b").unwrap();
    std::thread::sleep(Duration::from_millis(600));
    // `0` to the line start, `dw` deletes `xx `, Enter submits.
    p.send("0dw\r").unwrap();
    p.expect("mock: hello").unwrap();
    prompt(&mut p);
    exit(p);
}

/// After a turn: what the terminal shows from the turn summary to the
/// output of `/version` typed next, escape sequences and redrawn prompts
/// removed (a blank line shows as `\n\n`).
fn after_a_turn(p: &mut expectrl::session::OsSession) -> String {
    p.send_line("hello").unwrap();
    p.expect("tokens").unwrap();
    // Let the turn's last lines out before typing (they are part of what
    // the caller compares, so they are not consumed here).
    std::thread::sleep(Duration::from_millis(500));
    p.send_line("/version").unwrap();
    let found = p.expect("prometheus ").unwrap();
    let raw = String::from_utf8_lossy(found.before()).into_owned();
    without_csi(&raw).replace('\r', "").replace("› ", "")
}

/// `raw` without its CSI escape sequences (`ESC [ params letter`).
fn without_csi(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    out
}

#[test]
fn the_default_layout_separates_the_banner_and_turns() {
    let home = mock_home();
    let mut p = spawn(home.path(), &[]);
    let start = until_prompt(&mut p);
    assert!(start.contains("/help for commands"), "{start:?}");
    assert!(start.contains("\r\n\r\n"), "{start:?}");
    // The summary line, then a blank line (printed above a running read
    // or straight out, depending on timing: redrawn prompts are removed).
    let after = after_a_turn(&mut p);
    assert!(after.contains("\n\n"), "{after:?}");
    exit(p);
}

#[test]
fn the_compact_layout_has_a_short_banner_and_no_blank_lines() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "display.compact", "true"],
    ));
    let mut p = spawn(home.path(), &[]);
    let start = until_prompt(&mut p);
    assert!(start.contains("Prometheus "), "{start:?}");
    assert!(!start.contains("/help for commands"), "{start:?}");
    assert!(!start.contains("\r\n\r\n"), "{start:?}");
    let after = after_a_turn(&mut p);
    assert!(!after.contains("\n\n"), "{after:?}");
    exit(p);
}

/// A line submitted while the end of a turn is still being printed (the
/// answer, the summary, the blank line) must not block that output or the
/// REPL: `/exit` typed at once still ends the process.
#[test]
fn typing_while_a_turn_prints_its_end_never_blocks() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "display.bell_on_complete", "true"],
    ));
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("hello").unwrap();
    p.expect("mock: hello").unwrap();
    exit(p);
}
