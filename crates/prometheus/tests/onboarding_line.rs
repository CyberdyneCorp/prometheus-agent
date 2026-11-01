//! Onboarding at an interactive start (spec: cli, Onboarding state; Setup
//! wizard: offered when an interactive start has no model).

#![cfg(unix)]

mod common;

use common::pty::{exit, prompt, spawn};
use common::{mock_home, ok, run, temp_home};
use expectrl::Expect;

/// The banner's text up to the idle prompt.
fn start_text(home: &std::path::Path) -> String {
    let mut p = spawn(home, &[]);
    let found = p.expect("› ").unwrap();
    let text = String::from_utf8_lossy(found.before()).into_owned();
    exit(p);
    text
}

/// Scenario: Tip shown once (and again after a reset).
#[test]
fn a_one_time_tip_is_shown_once_per_profile() {
    let home = mock_home();
    assert!(start_text(home.path()).contains("Tip: type /help"));
    assert!(!start_text(home.path()).contains("Tip:"));
    ok(&run(home.path(), &["setup", "--reset-onboarding"]));
    assert!(start_text(home.path()).contains("Tip: type /help"));
}

#[test]
fn an_interactive_start_without_a_model_offers_setup() {
    let home = temp_home();
    let mut p = spawn(home.path(), &[]);
    p.expect("No model is configured for this profile. Set one up now? [Y/n]")
        .unwrap();
    p.send_line("y").unwrap();
    p.expect("provider number").unwrap();
    p.send_line("anthropic").unwrap();
    p.expect("model number or id").unwrap();
    p.send_line("claude-sonnet-4-5").unwrap();
    p.expect("value for ANTHROPIC_API_KEY").unwrap();
    p.send_line("sk-ant-test-0123456789abcdef").unwrap();
    p.expect("model set: anthropic/claude-sonnet-4-5").unwrap();
    p.expect("anthropic/claude-sonnet-4-5 · /help").unwrap();
    prompt(&mut p);
    exit(p);
}
