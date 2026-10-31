//! Tool loop guardrails, the empty-response and narration guards and the
//! run-budget wrap-up notice through mock-provider scripts (spec:
//! agent-turn-loop, Tool loop guardrails; Empty-response and narration
//! guards; Wall-clock run budget).

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use pmt_agent::turn::{ExitReason, TurnInput, WRAP_UP_NOTICE};
use pmt_core::id::SessionId;
use pmt_store::{Role, Source, Visibility};
use pmt_tools::ToolResult;

use crate::support::{
    FakeTools, Priced, Profile, agent, input_price, mock_model, mock_model_priced, options,
};

/// `terminal("make")` `times` times, then a text answer.
fn repeat_terminal(times: u32) -> String {
    format!(
        r#"
[[rule]]
times = {times}
[[rule.tool_calls]]
name = "terminal"
arguments = {{ command = "make" }}

[[rule]]
text = "I give up."
"#
    )
}

fn tool_texts(profile: &Profile, session: SessionId) -> Vec<String> {
    profile
        .store
        .messages(session, Visibility::Active)
        .unwrap()
        .into_iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.content.unwrap_or_default())
        .collect()
}

#[tokio::test]
async fn an_interactive_session_only_warns_about_repeated_failures() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, &repeat_terminal(6), "mock").await;
    let tools = FakeTools::answering(|_| ToolResult::error("make: *** [all] Error 2"));
    let run = agent(&profile, s, model, tools, |_| {});
    let result = run.turn(&profile, "build it").await;

    assert_eq!(result.exit_reason, ExitReason::Completed);
    let texts = tool_texts(&profile, s);
    assert_eq!(texts.len(), 6);
    assert!(!texts[0].contains("Warning"), "{}", texts[0]);
    for text in &texts[1..] {
        assert!(
            text.contains("[prometheus] Warning: this exact terminal call has failed"),
            "{text}"
        );
    }
}

#[tokio::test]
async fn a_cron_run_stops_at_the_fifth_identical_failure() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cron);
    let model = mock_model(&profile, &repeat_terminal(20), "mock").await;
    let tools = FakeTools::answering(|_| ToolResult::error("make: *** [all] Error 2"));
    let run = agent(&profile, s, model, tools.clone(), |setup| {
        setup.interactive = false;
    });
    let result = run.turn(&profile, "build it").await;

    assert_eq!(result.exit_reason, ExitReason::GuardrailStop);
    assert_eq!(tools.calls(), 5);
    assert_eq!(result.error.unwrap().code().code, "turn.guardrail_stop");
    assert!(
        result.notices[0].contains("failed 5 times"),
        "{:?}",
        result.notices
    );
    assert_eq!(run.end_reasons(), ["guardrail_stop"]);
}

#[tokio::test]
async fn hard_stops_can_be_turned_on_for_interactive_sessions() {
    let profile = Profile::new("[tool_loop_guardrails]\nhard_stop_enabled = true\n");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, &repeat_terminal(20), "mock").await;
    let tools = FakeTools::answering(|_| ToolResult::error("boom"));
    let run = agent(&profile, s, model, tools.clone(), |_| {});
    let result = run.turn(&profile, "go").await;
    assert_eq!(result.exit_reason, ExitReason::GuardrailStop);
    assert_eq!(tools.calls(), 5);
}

#[tokio::test]
async fn the_51st_web_search_of_a_turn_is_refused_unrun() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let mut script = String::from("[[rule]]\nstep = 0\n");
    for i in 0..51 {
        let _ = write!(
            script,
            "[[rule.tool_calls]]\nname = \"web_search\"\narguments = {{ query = \"q{i}\" }}\n"
        );
    }
    script.push_str("[[rule]]\nstep = 1\ntext = \"done\"\n");
    let model = mock_model(&profile, &script, "mock").await;
    let tools = FakeTools::new();
    let run = agent(&profile, s, model, tools.clone(), |_| {});
    let result = run.turn(&profile, "search a lot").await;

    assert_eq!(result.exit_reason, ExitReason::Completed);
    assert_eq!(
        tools.calls(),
        50,
        "no search was performed for the 51st call"
    );
    let texts = tool_texts(&profile, s);
    assert_eq!(texts.len(), 51);
    assert!(
        texts[50].contains("tool_loop_guardrails.loop_caps.max_web_searches"),
        "{}",
        texts[50]
    );
}

#[tokio::test]
async fn idempotent_calls_without_progress_are_flagged() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let script = r#"
[[rule]]
times = 2
[[rule.tool_calls]]
name = "lookup"
arguments = { q = "same" }

[[rule]]
text = "ok"
"#;
    let model = mock_model(&profile, script, "mock").await;
    let tools = Arc::new(FakeTools {
        idempotent: vec!["lookup".into()],
        ..FakeTools::plain()
    });
    let run = agent(&profile, s, model, tools, |_| {});
    run.turn(&profile, "go").await;
    let texts = tool_texts(&profile, s);
    assert!(!texts[0].contains("Warning"));
    assert!(
        texts[1].contains("returned the same result 2 times"),
        "{}",
        texts[1]
    );
}

/// Empty replies that still cost output tokens (reasoning only).
const ALWAYS_EMPTY: &str = "[[rule]]\nreasoning = \"thinking\"\nrepeat = true\n";

#[tokio::test]
async fn an_expensive_empty_reply_is_retried_once() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    // $1 per input token: every request costs far over the $0.25 threshold.
    let model =
        mock_model_priced(&profile, ALWAYS_EMPTY, "mock", Priced::At(input_price(1e6))).await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    let result = run.turn(&profile, "hi").await;
    assert_eq!(result.exit_reason, ExitReason::EmptyResponse);
    assert_eq!(model.requests().len(), 2, "one retry");
}

#[tokio::test]
async fn a_cheap_empty_reply_is_retried_three_times() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, ALWAYS_EMPTY, "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    let result = run.turn(&profile, "hi").await;
    assert_eq!(result.exit_reason, ExitReason::EmptyResponse);
    assert_eq!(model.requests().len(), 4);
    let roles: Vec<Role> = profile
        .store
        .messages(s, Visibility::Active)
        .unwrap()
        .iter()
        .map(|m| m.role)
        .collect();
    assert_eq!(
        roles,
        [Role::User, Role::Assistant],
        "empty replies are not stored"
    );
}

#[tokio::test]
async fn an_empty_reply_retry_can_recover() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let script = "[[rule]]\nreasoning = \"thinking\"\n\n[[rule]]\ntext = \"Here.\"\n";
    let model = mock_model(&profile, script, "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    let result = run.turn(&profile, "hi").await;
    assert_eq!(result.exit_reason, ExitReason::Completed);
    assert_eq!(result.final_response.as_deref(), Some("Here."));
}

const NARRATES: &str = r#"
[[rule]]
step = 0
text = "I read the issue. Let me check the failing test."

[[rule]]
step = 1
[[rule.tool_calls]]
name = "lookup"
arguments = { q = "test" }

[[rule]]
step = 2
text = "Fixed."
"#;

#[tokio::test]
async fn an_announced_action_without_a_call_is_nudged() {
    let profile = Profile::new("[agent]\nintent_continuation = true\n");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, NARRATES, "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    let result = run.turn(&profile, "fix it").await;

    assert_eq!(result.exit_reason, ExitReason::Completed);
    assert_eq!(result.final_response.as_deref(), Some("Fixed."));
    let second = &model.requests()[1];
    let last = second.messages.last().unwrap();
    assert_eq!(last.role, pmt_providers::request::Role::User);
    assert!(format!("{:?}", last.parts).contains("Continue now and call the tools"));
    // The nudge is a new row after the stored narration.
    assert_eq!(second.messages.len(), 3);
    assert!(format!("{:?}", second.messages[1].parts).contains("Let me check the failing test."));
}

#[tokio::test]
async fn nudges_are_capped_at_two_and_off_by_default_for_chat_wire_models() {
    let script = "[[rule]]\ntext = \"Let me look into it.\"\nrepeat = true\n";
    let profile = Profile::new("[agent]\nintent_continuation = true\n");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, script, "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    let result = run.turn(&profile, "go").await;
    assert_eq!(result.exit_reason, ExitReason::Completed);
    assert_eq!(
        model.requests().len(),
        3,
        "two nudges, then the answer stands"
    );

    // `auto` (the default) applies to Responses-wire models only; the mock
    // route speaks chat completions.
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, script, "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    run.turn(&profile, "go").await;
    assert_eq!(model.requests().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_wrap_up_notice_rides_one_tool_result_at_80_percent() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let script = r#"
[[rule]]
times = 10
[[rule.tool_calls]]
name = "lookup"
arguments = { q = "more" }

[[rule]]
text = "Final deliverable."
"#;
    let model = mock_model(&profile, script, "mock").await;
    let tools = Arc::new(FakeTools {
        delay: Duration::from_millis(900),
        ..FakeTools::plain()
    });
    let run = agent(&profile, s, model, tools, |_| {});
    let mut options = options();
    options.run_budget = Some(Duration::from_secs(10));
    let result = run
        .turn_with(&profile, TurnInput::text("work"), options)
        .await;

    assert_eq!(result.exit_reason, ExitReason::Completed);
    let texts = tool_texts(&profile, s);
    let marked: Vec<usize> = texts
        .iter()
        .enumerate()
        .filter(|(_, t)| t.contains(WRAP_UP_NOTICE))
        .map(|(i, _)| i)
        .collect();
    // Batch 9 ends at 8.1 s, the first past 8 s (80% of 10 s); batch 10
    // carries no second notice.
    assert_eq!(marked, [8]);
}

#[tokio::test]
async fn without_a_run_budget_no_notice_is_added() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, &repeat_terminal(2), "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    run.turn(&profile, "go").await;
    assert!(
        tool_texts(&profile, s)
            .iter()
            .all(|t| !t.contains(WRAP_UP_NOTICE))
    );
}
