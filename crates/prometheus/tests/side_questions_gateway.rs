//! `/btw` side questions in a messaging chat served by the real daemon
//! (spec: agent-turn-loop, Side questions with /btw): the answer arrives as
//! a separate chat message labelled as a side answer while the turn runs,
//! and the stored transcript holds neither the question nor the answer.

#![cfg(feature = "gateway")]

mod common;

use self::common::gateway::{Daemon, gateway_home_with, send, texts, wait_said};

/// `slow` takes a few seconds; the side question is answered at once.
const RULES: &str = "[[rule]]\nuser = \"which branch\"\nrepeat = true\ntext = \"We are on main.\"\n\n\
                     [[rule]]\nuser_regex = '^slow$'\nrepeat = true\nlatency_ms = 6000\ntext = \"slow done\"\n";

/// Scenario: Busy session.
#[test]
fn a_side_answer_reaches_the_chat_while_the_turn_runs_and_is_never_stored() {
    let home = gateway_home_with(RULES, "");
    let home = home.path();
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "slow");
    common::wait_for_turn_start(home);
    send(home, "default", "u1", "/btw which branch are we on?");
    wait_said(home, "default", "side answer: We are on main.");
    let said = texts(home, "default");
    assert!(
        !said.iter().any(|t| t.contains("slow done")),
        "the side answer came while the turn ran: {said:?}"
    );
    let answer = said
        .iter()
        .find(|t| t.contains("side answer: We are on main."))
        .unwrap();
    assert!(
        !answer.contains("pending"),
        "a message of its own, not the pending notice: {said:?}"
    );
    wait_said(home, "default", "slow done");
    daemon.stop(home);

    let session = common::newest_session(home);
    let all = serde_json::to_string(&common::session_messages(home, &session)).unwrap();
    assert!(!all.contains("which branch"), "{all}");
    assert!(!all.contains("We are on main"), "{all}");
    assert!(all.contains("slow done"), "{all}");
}
