//! The CLI in a Windows console (spec: cli, Windows console behaviour):
//! the console's code page comes back after `prometheus` exits.

#![cfg(windows)]

mod common;

use std::os::windows::process::CommandExt as _;

/// `CREATE_NEW_CONSOLE`: the shell gets a console of its own, whatever
/// the test runner has.
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

/// Scenario: Code page restored. In a console set to code page 437,
/// `prometheus` runs (switching it to UTF-8 for its lifetime) and exits;
/// `chcp` then reports 437 again.
#[test]
fn the_console_code_page_is_restored_when_prometheus_exits() {
    let home = common::temp_home();
    let report = home.path().join("codepage.txt");
    let exe = env!("CARGO_BIN_EXE_prometheus");
    let script = format!(
        "chcp 437 >nul & \"{exe}\" version >nul & chcp > \"{}\"",
        report.display()
    );
    let status = std::process::Command::new("cmd")
        .raw_arg(format!("/d /s /c \"{script}\""))
        .creation_flags(CREATE_NEW_CONSOLE)
        .status()
        .unwrap();
    assert!(status.success(), "{status:?}");
    let text = std::fs::read_to_string(&report).unwrap();
    assert!(text.contains("437"), "the code page after exit: {text}");
}
