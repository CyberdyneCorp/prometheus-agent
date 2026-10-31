//! Iteration limits, per-agent iteration budgets, token and cost budgets
//! and the summary handoff (spec: agent-turn-loop, Single documented
//! iteration limit; Per-agent iteration budget; Budget-exhaustion summary
//! handoff; Per-turn and per-session token and cost budgets; Unpriced
//! models fail closed under a cost budget).

use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};

use pmt_agent::turn::{ExitReason, HANDOFF_NOTICE};
use pmt_core::id::SessionId;
use pmt_obs::trajectory::events::Event;
use pmt_obs::usage::{Call, UsageRecorder};
use pmt_store::{LineageKind, NewSession, Role, Source, Tokens, Visibility};

use crate::support::{FakeTools, Profile, agent, mock_model, mock_model_with};

/// A model that keeps calling tools.
const ENDLESS_TOOLS: &str = r#"
[[rule]]
step = 0
[[rule.tool_calls]]
name = "lookup"
arguments = { q = "a" }

[[rule]]
step = 1
[[rule.tool_calls]]
name = "lookup"
arguments = { q = "b" }

[[rule]]
step = 2
text = "Summary: looked up a and b."
"#;

/// One tool round, then an answer (the handoff, when budgets stop it).
const ONE_ROUND: &str = r#"
[[rule]]
step = 0
[[rule.tool_calls]]
name = "lookup"
arguments = { q = "big" }

[[rule]]
step = 1
text = "Summary so far."
"#;

fn stored_roles(profile: &Profile, session: SessionId) -> Vec<Role> {
    profile
        .store
        .messages(session, Visibility::Active)
        .unwrap()
        .iter()
        .map(|m| m.role)
        .collect()
}

#[tokio::test]
async fn the_iteration_limit_hands_off_to_a_toolless_summary() {
    let profile = Profile::new("[agent]\nmax_iterations = 2\n");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, ENDLESS_TOOLS, "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    let result = run.turn(&profile, "research").await;

    assert_eq!(result.exit_reason, ExitReason::IterationLimit);
    assert_eq!(
        result.final_response.as_deref(),
        Some("Summary: looked up a and b.")
    );
    assert_eq!(result.iterations, 2, "the handoff is exempt from the limit");
    assert_eq!(result.usage.calls, 3, "the handoff counts toward usage");
    let requests = model.requests();
    let handoff = requests.last().unwrap();
    assert!(handoff.tools.is_empty(), "the handoff request is toolless");
    assert!(requests[0].tools.len() == 1);
    let notice = handoff.messages.last().unwrap();
    assert_eq!(notice.role, pmt_providers::request::Role::User);
    assert!(
        matches!(&notice.parts[0], pmt_providers::request::Part::Text { text } if text == HANDOFF_NOTICE)
    );
    assert_eq!(
        stored_roles(&profile, s),
        [
            Role::User,
            Role::Assistant,
            Role::Tool,
            Role::Assistant,
            Role::Tool,
            Role::User,
            Role::Assistant
        ]
    );
    assert_eq!(run.end_reasons(), ["iteration_limit"]);
}

#[tokio::test]
async fn an_empty_handoff_reply_is_retried_once() {
    let profile = Profile::new("[agent]\nmax_iterations = 1\n");
    let s = profile.session(Source::Cli);
    let script = r#"
[[rule]]
step = 0
[[rule.tool_calls]]
name = "lookup"
arguments = { q = "a" }

[[rule]]
step = 1
text = ""

[[rule]]
step = 1
text = "Second try."
"#;
    let model = mock_model(&profile, script, "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    let result = run.turn(&profile, "go").await;
    assert_eq!(result.exit_reason, ExitReason::IterationLimit);
    assert_eq!(result.final_response.as_deref(), Some("Second try."));
    assert_eq!(model.requests().len(), 3);
}

#[tokio::test]
async fn zero_iterations_means_unlimited() {
    let profile = Profile::new("[agent]\nmax_iterations = 0\n");
    let s = profile.session(Source::Cli);
    let mut script = String::new();
    for step in 0..6 {
        let _ = write!(
            script,
            "[[rule]]\nstep = {step}\n[[rule.tool_calls]]\nname = \"lookup\"\narguments = {{ q = \"{step}\" }}\n\n"
        );
    }
    script.push_str("[[rule]]\nstep = 6\ntext = \"done\"\n");
    let model = mock_model(&profile, &script, "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    let result = run.turn(&profile, "go").await;
    assert_eq!(result.exit_reason, ExitReason::Completed);
    assert_eq!(result.iterations, 7);
}

#[tokio::test]
async fn the_default_limit_is_500_and_strings_are_refused() {
    assert_eq!(pmt_config::Config::default().agent.max_iterations, 500);
    let err = pmt_config::check(
        std::path::Path::new("config.toml"),
        "[agent]\nmax_iterations = \"unlimited\"\n",
    )
    .unwrap_err();
    assert!(
        err.message()
            .contains("agent.max_iterations: expected integer"),
        "{}",
        err.message()
    );
    assert_eq!(err.exit_code(), 78);
}

#[tokio::test]
async fn a_child_exhausts_its_own_budget_only() {
    let profile = Profile::new("[agent]\nmax_iterations = 400\n");
    let parent = profile.session(Source::Cli);
    let child = profile
        .store
        .create_session(&NewSession::child(
            Source::Subagent,
            parent,
            LineageKind::Delegate,
        ))
        .unwrap()
        .id;
    let child_model = mock_model(&profile, ENDLESS_TOOLS, "mock").await;
    let child_run = agent(&profile, child, child_model, FakeTools::new(), |setup| {
        setup.max_iterations = Some(2);
    });
    let child_result = child_run.turn(&profile, "subtask").await;
    assert_eq!(child_result.exit_reason, ExitReason::IterationLimit);
    assert_eq!(child_result.iterations, 2);

    let parent_model = mock_model(&profile, ENDLESS_TOOLS, "mock").await;
    let parent_run = agent(&profile, parent, parent_model, FakeTools::new(), |_| {});
    let parent_result = parent_run.turn(&profile, "task").await;
    assert_eq!(parent_result.exit_reason, ExitReason::Completed);
    assert_eq!(parent_result.iterations, 3);
}

#[tokio::test]
async fn a_cost_cap_skips_the_request_and_hands_off() {
    let profile = Profile::new(
        "[budgets]\nturn_usd = 0.10\n\n[models.\"mock/mock\".price]\ninput = 100.0\noutput = 100.0\ncache_read = 0.0\ncache_write = 0.0\n",
    );
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, ONE_ROUND, "mock").await;
    let tools = Arc::new(FakeTools {
        output: "x".repeat(8_000),
        ..FakeTools::plain()
    });
    let run = agent(&profile, s, model.clone(), tools, |_| {});
    let result = run.turn(&profile, "go").await;

    assert_eq!(result.exit_reason, ExitReason::CostBudgetExhausted);
    assert_eq!(result.final_response.as_deref(), Some("Summary so far."));
    assert_eq!(result.error.unwrap().code().code, "budget.exceeded");
    let requests = model.requests();
    assert_eq!(
        requests.len(),
        2,
        "the second tool-bearing request was skipped"
    );
    assert!(requests[1].tools.is_empty(), "only the handoff was sent");
    let exceeded: Vec<_> = run
        .events
        .events()
        .into_iter()
        .filter_map(|e| match e {
            Event::BudgetExceeded(b) => Some(b),
            _ => None,
        })
        .collect();
    assert_eq!(exceeded.len(), 1);
    assert_eq!(
        (exceeded[0].scope.as_str(), exceeded[0].unit.as_str()),
        ("turn", "usd")
    );
}

#[tokio::test]
async fn child_spend_counts_toward_the_turn_token_budget() {
    let profile = Profile::new("[budgets]\nturn_tokens = 100000\n");
    let parent = profile.session(Source::Cli);
    let child = profile
        .store
        .create_session(&NewSession::child(
            Source::Subagent,
            parent,
            LineageKind::Delegate,
        ))
        .unwrap()
        .id;
    let model = mock_model(&profile, ONE_ROUND, "mock").await;
    let recorder = Arc::new(UsageRecorder::new(
        profile.store.clone(),
        Arc::new(pmt_obs::usage::ConfigPrices::from_config(
            profile.scope.config(),
        )),
    ));
    let child_spent = Arc::new(Mutex::new(false));
    let flag = child_spent.clone();
    let tools = Arc::new(FakeTools {
        // ~20,000 tokens of tool output make the next request large.
        output: "y".repeat(80_000),
        effect: Some(Box::new(move || {
            // The delegated child spent 90,000 tokens during this turn.
            let mut call = Call::new(child, "main", "mock", "mock");
            call.tokens = Tokens {
                input: 90_000,
                ..Tokens::default()
            };
            recorder.record(call).unwrap();
            *flag.lock().unwrap_or_else(PoisonError::into_inner) = true;
        })),
        ..FakeTools::plain()
    });
    let run = agent(&profile, parent, model, tools, |_| {});
    let result = run.turn(&profile, "go").await;
    assert!(*child_spent.lock().unwrap());
    assert_eq!(result.exit_reason, ExitReason::TokenBudgetExhausted);
    assert_eq!(result.final_response.as_deref(), Some("Summary so far."));
    assert_eq!(result.error.unwrap().exit_code(), 3);
}

#[tokio::test]
async fn an_unpriced_model_under_a_cost_budget_is_never_called() {
    let profile = Profile::new("[budgets]\nsession_usd = 5.0\n");
    let s = profile.session(Source::Cli);
    let model = mock_model_with(&profile, "[[rule]]\ntext = \"x\"\n", "mock", true).await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    let result = run.turn(&profile, "hello").await;

    assert_eq!(result.exit_reason, ExitReason::CostBudgetExhausted);
    let err = result.error.unwrap();
    assert_eq!(err.code().code, "budget.unpriced_model");
    assert!(err.message().contains("'mock'"), "{}", err.message());
    assert_eq!(err.exit_code(), 3);
    assert!(model.requests().is_empty(), "no model call, no handoff");
    assert_eq!(result.final_response, None);
    // Only the failed-turn boundary row follows the user's message.
    assert_eq!(stored_roles(&profile, s), [Role::User, Role::Assistant]);
}
