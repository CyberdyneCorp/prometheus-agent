//! Interrupting a one-shot run on Windows (spec: cli, Stable exit codes;
//! replay-and-evaluation, Built-in smoke suite: the `interrupt` task).
//! Ctrl+Break is the console event a parent can send to one child: the
//! child runs in a console process group of its own and gets Ctrl+Break
//! there, as the smoke suite's interrupt task does. The run stops its
//! turn and exits 130.

#![cfg(windows)]

mod common;

use std::process::Stdio;

use common::{command, mock_home, wait_for_turn_start};

#[test]
fn ctrl_break_stops_a_one_shot_run_with_exit_130() {
    let home = mock_home();
    let script = home.path().join("script.toml");
    std::fs::write(
        &script,
        "[[rule]]\nrepeat = true\nlatency_ms = 20000\ntext = \"slow\"\n",
    )
    .unwrap();
    let mut cmd = command(home.path());
    cmd.args(["chat", "-z", "hi", "--mock-script"])
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    pmt_sandbox::console::new_process_group(&mut cmd);
    let child = cmd.spawn().unwrap();
    wait_for_turn_start(home.path());
    pmt_sandbox::console::ctrl_break(child.id()).unwrap();
    let started = std::time::Instant::now();
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(130),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Well before the 20 s model call would have finished.
    assert!(started.elapsed() < std::time::Duration::from_secs(15));
}
