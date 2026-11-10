//! Prefill messages (spec: agent-turn-loop, Prefill messages).

use pmt_agent::turn::TurnInput;
use pmt_providers::request::{Part, Request};
use pmt_store::{Source, Visibility};

use crate::support::{FakeTools, Profile, agent, mock_model, options};

const ANSWER: &str = "[[rule]]\nrepeat = true\ntext = \"ok\"\n";

fn texts(request: &Request) -> Vec<String> {
    request
        .messages
        .iter()
        .map(|m| match &m.parts[0] {
            Part::Text { text } => text.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

fn write_prefill(profile: &Profile, answer: &str) {
    std::fs::write(
        profile.dir().join("fewshot.json"),
        format!(
            r#"[{{"role":"user","content":"What is 2+2?"}},{{"role":"assistant","content":"{answer}"}}]"#
        ),
    )
    .unwrap();
}

#[tokio::test]
async fn prefill_is_frozen_at_session_start_and_never_stored() {
    let profile = Profile::new("[prompt]\nprefill_messages_file = \"fewshot.json\"\n");
    write_prefill(&profile, "4");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, ANSWER, "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    run.turn(&profile, "first").await;
    write_prefill(&profile, "four");
    run.turn(&profile, "second").await;

    let requests = model.requests();
    assert_eq!(texts(&requests[0])[..3], ["What is 2+2?", "4", "first"]);
    assert_eq!(
        texts(&requests[1])[..2],
        ["What is 2+2?", "4"],
        "the running session keeps the original"
    );
    let stored = profile.store.messages(s, Visibility::All).unwrap();
    assert!(
        stored
            .iter()
            .all(|m| m.content.as_deref() != Some("What is 2+2?")),
        "never transcript rows"
    );

    // A new session reads the edited file.
    let fresh = profile.session(Source::Cli);
    let model = mock_model(&profile, ANSWER, "mock").await;
    let run = agent(&profile, fresh, model.clone(), FakeTools::new(), |_| {});
    run.turn(&profile, "third").await;
    assert_eq!(texts(&model.requests()[0])[..2], ["What is 2+2?", "four"]);
}

#[tokio::test]
async fn a_prefill_with_another_role_fails_the_session_start() {
    let profile = Profile::new("[prompt]\nprefill_messages_file = \"fewshot.json\"\n");
    std::fs::write(
        profile.dir().join("fewshot.json"),
        r#"[{"role":"system","content":"be terse"}]"#,
    )
    .unwrap();
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, ANSWER, "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    let err = run
        .agent
        .run_turn(Some(&profile.scope), s, TurnInput::text("hi"), options())
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "config.invalid");
    assert_eq!(err.exit_code(), 78);
    assert!(model.requests().is_empty());
}
