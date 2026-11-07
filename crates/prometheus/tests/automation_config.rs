//! The session automation sections of `config.toml` (spec:
//! session-automation, Automation configuration).

mod common;

use common::{mock_home as temp_home, run, stderr};

/// Scenario: Inverted self-paced bounds.
#[test]
fn an_inverted_self_paced_range_fails_validation_naming_both_keys() {
    let home = temp_home();
    common::append_config(
        home.path(),
        "\n[loops]\nself_paced_floor_secs = 600\nself_paced_ceiling_secs = 300\n",
    );
    let out = run(home.path(), &["config", "validate"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    let text = stderr(&out);
    assert!(text.contains("loops.self_paced_floor_secs"), "{text}");
    assert!(text.contains("loops.self_paced_ceiling_secs"), "{text}");
}

/// Out-of-range values are refused, never clamped.
#[test]
fn an_out_of_range_turn_budget_is_refused() {
    let home = temp_home();
    common::append_config(home.path(), "\n[goals]\nmax_turns = 0\n");
    let out = run(home.path(), &["config", "validate"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(stderr(&out).contains("goals.max_turns"), "{}", stderr(&out));
}
