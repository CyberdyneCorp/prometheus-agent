//! Composing in the line interface through a pseudo-terminal (spec: cli,
//! Image attachments; External editor composition; Shell escape in the
//! REPL; Job-control suspend).

#![cfg(unix)]

mod common;

use std::time::Duration;

use common::pty::{exit, prompt, script, shell, spawn, spawn_with};
use common::{PNG, active_messages, mock_home};
use expectrl::{ControlCode, Expect};

fn user_contents(home: &std::path::Path) -> Vec<String> {
    active_messages(home)
        .into_iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn image_attaches_to_the_next_prompt_lists_and_clears() {
    let home = mock_home();
    std::fs::write(home.path().join("a.png"), PNG).unwrap();
    std::fs::write(home.path().join("b.txt"), "text").unwrap();
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("/image").unwrap();
    p.expect("no pending images").unwrap();
    prompt(&mut p);
    p.send_line("/image b.txt").unwrap();
    p.expect("not a PNG, JPEG, GIF or WebP image").unwrap();
    prompt(&mut p);
    p.send_line("/image a.png").unwrap();
    p.expect("attached a.png to the next prompt (1 pending)")
        .unwrap();
    prompt(&mut p);
    p.send_line("/image clear").unwrap();
    p.expect("dropped 1 pending image(s)").unwrap();
    prompt(&mut p);
    p.send_line("/image a.png").unwrap();
    prompt(&mut p);
    p.send_line("look").unwrap();
    p.expect("mock: look").unwrap();
    prompt(&mut p);
    p.send_line("/image").unwrap();
    p.expect("no pending images").unwrap();
    prompt(&mut p);
    exit(p);
    let users: Vec<_> = active_messages(home.path())
        .into_iter()
        .filter(|m| m["role"] == "user")
        .collect();
    assert_eq!(users[0]["metadata"]["images"][0]["name"], "a.png");
}

/// Scenario: Empty file cancels.
#[test]
fn an_editor_that_saves_nothing_sends_nothing() {
    let home = mock_home();
    let editor = script(home.path(), "noop.sh", "exit 0");
    let mut p = spawn_with(home.path(), &[], &[("VISUAL", &editor)]);
    prompt(&mut p);
    p.send_line("/prompt").unwrap();
    p.expect("empty prompt; nothing sent").unwrap();
    prompt(&mut p);
    exit(p);
    assert!(user_contents(home.path()).is_empty());
    let compose = home.path().join("profiles/default/cache/compose");
    assert_eq!(std::fs::read_dir(compose).unwrap().count(), 0);
}

#[test]
fn the_editor_result_is_the_next_message_and_ctrl_x_ctrl_e_keeps_the_buffer() {
    let home = mock_home();
    let editor = script(
        home.path(),
        "append.sh",
        "printf ' from the editor' >> \"$1\"",
    );
    let mut p = spawn_with(home.path(), &[], &[("EDITOR", &editor)]);
    prompt(&mut p);
    p.send_line("/compose draft").unwrap();
    p.expect("mock: draft from the editor").unwrap();
    prompt(&mut p);
    p.send("half").unwrap();
    p.send(ControlCode::Cancel).unwrap(); // Ctrl+X
    p.send(ControlCode::Enquiry).unwrap(); // Ctrl+E
    p.expect("mock: half from the editor").unwrap();
    prompt(&mut p);
    exit(p);
}

/// Scenario: Output kept local; `!!` shares it with the next message.
#[test]
fn shell_escape_output_stays_local_unless_doubled() {
    let home = mock_home();
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("!echo kept-local-$((40+2))").unwrap();
    p.expect("kept-local-42").unwrap();
    prompt(&mut p);
    p.send_line("first").unwrap();
    p.expect("mock: first").unwrap();
    prompt(&mut p);
    p.send_line("!!echo shared-$((1+1))").unwrap();
    p.expect("shared-2").unwrap();
    prompt(&mut p);
    p.send_line("second").unwrap();
    p.expect("mock: second").unwrap();
    prompt(&mut p);
    exit(p);
    let users = user_contents(home.path());
    assert!(
        users.iter().all(|u| !u.contains("kept-local-42")),
        "{users:?}"
    );
    assert!(
        users[1].contains("$ echo shared-$((1+1))") && users[1].contains("shared-2"),
        "{users:?}"
    );
}

fn slow_script(home: &std::path::Path) -> String {
    let path = home.join("slow.toml");
    std::fs::write(
        &path,
        "[[rule]]\nuser = \"slow\"\nrepeat = true\nlatency_ms = 2000\ntext = \"done slowly\"\n",
    )
    .unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn shell_escape_and_suspend_are_refused_while_a_turn_runs() {
    let home = mock_home();
    let slow = slow_script(home.path());
    let mut p = spawn(home.path(), &["--mock-script", &slow]);
    prompt(&mut p);
    p.send_line("slow").unwrap();
    std::thread::sleep(Duration::from_millis(400));
    p.send_line("!ls").unwrap();
    p.expect("shell escape is unavailable while the agent is working")
        .unwrap();
    // The busy prompt is read again (Ctrl+Z between reads is the
    // terminal's own signal).
    prompt(&mut p);
    p.send(ControlCode::Substitute).unwrap(); // Ctrl+Z
    p.expect("cannot suspend while the agent is working")
        .unwrap();
    p.expect("done slowly").unwrap();
    prompt(&mut p);
    exit(p);
}

/// Scenario: Resume after fg.
#[test]
fn ctrl_z_suspends_and_fg_redraws_the_buffer() {
    let home = mock_home();
    let mut sh = shell(home.path());
    sh.expect("$ ").unwrap();
    sh.send_line(format!("{} --home {}", common::BIN, home.path().display()))
        .unwrap();
    sh.expect("› ").unwrap();
    sh.send("half typed").unwrap();
    sh.expect("half typed").unwrap();
    sh.send(ControlCode::Substitute).unwrap();
    sh.expect("$ ").unwrap();
    sh.send_line("fg").unwrap();
    sh.expect("half typed").unwrap();
    sh.send_line("").unwrap();
    sh.expect("mock: half typed").unwrap();
    sh.expect("› ").unwrap();
    sh.send_line("/exit").unwrap();
    sh.expect("$ ").unwrap();
    sh.send_line("exit").unwrap();
}
