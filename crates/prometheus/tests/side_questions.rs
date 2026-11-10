//! `/btw` side questions in the line interface while a turn runs (spec:
//! agent-turn-loop, Side questions with /btw).

#![cfg(unix)]

mod common;

use common::pty;
use expectrl::Expect;

const SCRIPT: &str = r#"
[[rule]]
user = "which branch"
text = "We are on main."

[[rule]]
step = 0
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "sleep 4" }

[[rule]]
step = 1
text = "finished the work"
"#;

#[test]
fn a_side_question_is_answered_while_the_turn_runs_and_never_stored() {
    let home = common::mock_home();
    let script = home.path().join("mock.toml");
    std::fs::write(&script, SCRIPT).unwrap();
    let mut p = pty::spawn(
        home.path(),
        &["--yolo", "--mock-script", script.to_str().unwrap()],
    );
    pty::prompt(&mut p);
    p.send_line("do the work").unwrap();
    pty::wait_for_turns(&mut p, home.path(), 1);
    p.send_line("/btw which branch are we on?").unwrap();
    p.expect("side answer: We are on main.").unwrap();
    p.expect("finished the work").unwrap();
    pty::prompt(&mut p);
    pty::exit(p);

    let messages = common::session_messages(home.path(), &common::newest_session(home.path()));
    let all = serde_json::to_string(&messages).unwrap();
    assert!(!all.contains("which branch"), "{all}");
    assert!(!all.contains("We are on main"), "{all}");
    assert!(all.contains("finished the work"));
}
