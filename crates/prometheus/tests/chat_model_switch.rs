//! `/model` in the line interface driven through a pseudo-terminal (spec:
//! model-providers, /model switch command: `--once` applies to the next
//! turn only, `--session` until the session ends).

#![cfg(unix)]

mod common;

use std::path::Path;

use common::pty::{exit, prompt, spawn};
use common::{mock_home, ok, run};
use expectrl::Expect;
use expectrl::session::OsSession;

/// A mock home on `mock/alpha` whose script answers with the model that
/// served the turn.
fn two_models(home: &Path) -> String {
    ok(&run(home, &["config", "set", "model.default", "alpha"]));
    let path = home.join("models.toml");
    std::fs::write(
        &path,
        "[[rule]]\nmodel = \"alpha\"\nrepeat = true\ntext = \"answered by alpha\"\n\n\
         [[rule]]\nmodel = \"beta\"\nrepeat = true\ntext = \"answered by beta\"\n",
    )
    .unwrap();
    path.to_string_lossy().into_owned()
}

/// Sends `text` as a turn and waits for `model`'s answer and the prompt.
fn turn_on(p: &mut OsSession, text: &str, model: &str) {
    p.send_line(text).unwrap();
    p.expect(format!("answered by {model}").as_str()).unwrap();
    prompt(p);
}

#[test]
fn a_once_switch_serves_one_turn_then_the_route_goes_back() {
    let home = mock_home();
    let script = two_models(home.path());
    let mut p = spawn(home.path(), &["--no-color", "--mock-script", &script]);
    prompt(&mut p);
    turn_on(&mut p, "first", "alpha");
    p.send_line("/model mock/beta --once").unwrap();
    p.expect("for the next turn only").unwrap();
    prompt(&mut p);
    p.send_line("second").unwrap();
    p.expect("answered by beta").unwrap();
    p.expect("model: back to mock/alpha").unwrap();
    prompt(&mut p);
    turn_on(&mut p, "third", "alpha");
    turn_on(&mut p, "fourth", "alpha");
    exit(p);
}

#[test]
fn a_session_switch_lasts_and_a_once_switch_returns_to_it() {
    let home = mock_home();
    let script = two_models(home.path());
    let mut p = spawn(home.path(), &["--no-color", "--mock-script", &script]);
    prompt(&mut p);
    p.send_line("/model mock/beta --session").unwrap();
    p.expect("until the session ends").unwrap();
    prompt(&mut p);
    turn_on(&mut p, "first", "beta");
    turn_on(&mut p, "second", "beta");
    p.send_line("/model mock/alpha --once").unwrap();
    p.expect("for the next turn only").unwrap();
    prompt(&mut p);
    turn_on(&mut p, "third", "alpha");
    turn_on(&mut p, "fourth", "beta");
    exit(p);
}

/// Spec: tool-system, Model-dependent schema variants — Variant frozen for
/// the session: a session started on an OpenAI-family model is offered
/// the V4A `patch` schema; after `/model` switches to a Claude model
/// mid-session it is still offered (the surface is frozen), while a new
/// session on the Claude model gets the replace-only schema.
#[test]
fn a_model_switch_keeps_the_frozen_patch_variant() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "model.default", "gpt-5"],
    ));
    let path = home.path().join("variants.toml");
    std::fs::write(
        &path,
        "[[rule]]\ntools = '\"v4a\"'\nrepeat = true\ntext = \"patch offers v4a\"\n\n\
         [[rule]]\nrepeat = true\ntext = \"patch offers replace only\"\n",
    )
    .unwrap();
    let script = path.to_string_lossy().into_owned();
    let mut p = spawn(home.path(), &["--no-color", "--mock-script", &script]);
    prompt(&mut p);
    p.send_line("first").unwrap();
    p.expect("patch offers v4a").unwrap();
    prompt(&mut p);
    p.send_line("/model mock/claude-sonnet-4 --session")
        .unwrap();
    p.expect("until the session ends").unwrap();
    prompt(&mut p);
    p.send_line("second").unwrap();
    p.expect("patch offers v4a").unwrap();
    prompt(&mut p);
    exit(p);

    let out = run(
        home.path(),
        &[
            "-z",
            "third",
            "--model",
            "mock/claude-sonnet-4",
            "--mock-script",
            &script,
        ],
    );
    assert!(
        common::stdout(&out).contains("patch offers replace only"),
        "{}",
        common::stderr(&out)
    );
}
