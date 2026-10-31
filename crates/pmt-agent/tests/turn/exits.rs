//! Every exit reason through mock-provider scripts (spec: agent-turn-loop,
//! Turn entry contract; Typed exit reasons; Single active turn per
//! session; Iteration phases).

use std::sync::Arc;
use std::time::Duration;

use pmt_agent::turn::model::RequestedCall;
use pmt_agent::turn::tools::ToolOutcome;
use pmt_agent::turn::{ExitReason, Guardrail, Issuer, TurnInput};
use pmt_obs::trajectory::events::Event;
use pmt_store::{Holder, LeaseKind, Role, Source, Visibility};

use crate::support::{FakeTools, Profile, agent, mock_model, options};

const TOOL_THEN_ANSWER: &str = r#"
[[rule]]
step = 0
[[rule.tool_calls]]
name = "lookup"
arguments = { q = "weather" }

[[rule]]
step = 1
text = "It is sunny."
"#;

/// A model that answers slowly enough to be interrupted.
const SLOW: &str = r#"
[[rule]]
text = "late answer"
latency_ms = 3000
repeat = true
"#;

#[tokio::test]
async fn a_text_answer_completes_the_turn() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, "[[rule]]\ntext = \"Hello there.\"\n", "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    let result = run.turn(&profile, "hi").await;

    assert_eq!(result.exit_reason, ExitReason::Completed);
    assert_eq!(result.final_response.as_deref(), Some("Hello there."));
    assert_eq!(result.messages_appended.len(), 2);
    assert_eq!(result.iterations, 1);
    assert_eq!(result.usage.calls, 1);
    let id = result.turn_id.as_str();
    assert_eq!(id.len(), 36, "{id}");
    assert_eq!(&id[14..15], "7", "UUIDv7 version nibble: {id}");
    assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"), "{id}");
    assert_eq!(run.end_reasons(), ["completed"]);
    let kinds = run.kinds();
    assert_eq!(kinds.first(), Some(&"prompt.rebuilt"));
    assert!(kinds.contains(&"model.call"), "{kinds:?}");
    assert_eq!(kinds.last(), Some(&"turn.end"));
}

#[tokio::test]
async fn tool_calls_loop_back_until_the_answer() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, TOOL_THEN_ANSWER, "mock").await;
    let tools = FakeTools::new();
    let run = agent(&profile, s, model.clone(), tools.clone(), |_| {});
    let result = run.turn(&profile, "weather?").await;

    assert_eq!(result.exit_reason, ExitReason::Completed);
    assert_eq!(result.final_response.as_deref(), Some("It is sunny."));
    assert_eq!(result.iterations, 2);
    assert_eq!(tools.calls(), 1);
    let roles: Vec<Role> = profile
        .store
        .messages(s, Visibility::Active)
        .unwrap()
        .iter()
        .map(|m| m.role)
        .collect();
    assert_eq!(
        roles,
        [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
    );
    let second = &model.requests()[1];
    assert_eq!(second.messages.len(), 3, "the tool result was sent back");
    let steps: Vec<_> = run
        .events
        .events()
        .into_iter()
        .filter_map(|e| match e {
            Event::TurnStep(step) => Some(step),
            _ => None,
        })
        .collect();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].iteration, 1);
    assert_eq!(steps[0].calls[0].tool, "lookup");
    assert_eq!(steps[0].calls[0].outcome, "ok");
    assert!(steps[0].calls[0].args_digest.starts_with("sha256:"));
}

#[tokio::test]
async fn cli_and_gateway_sessions_share_the_loop() {
    let profile = Profile::new("");
    for source in [Source::Cli, Source::Gateway] {
        let s = profile.session(source);
        let model = mock_model(&profile, "[[rule]]\ntext = \"ok\"\n", "mock").await;
        let run = agent(&profile, s, model, FakeTools::new(), |_| {});
        let result = run.turn(&profile, "hello").await;
        assert_eq!(result.exit_reason, ExitReason::Completed);
        assert!(!result.turn_id.as_str().is_empty());
    }
}

#[tokio::test]
async fn a_turn_without_a_profile_scope_is_refused() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, "[[rule]]\ntext = \"x\"\n", "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    let err = run
        .agent
        .run_turn(None, s, TurnInput::text("hi"), options())
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "turn.scope_required");
    assert_eq!(err.message(), "profile scope required");
    assert!(model.requests().is_empty());
    assert!(
        profile
            .store
            .messages(s, Visibility::All)
            .unwrap()
            .is_empty()
    );

    let other = profile.sibling("work", "");
    let err = run
        .agent
        .run_turn(Some(&other.scope), s, TurnInput::text("hi"), options())
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "turn.scope_mismatch");
}

#[tokio::test]
async fn a_busy_lease_ends_the_turn_without_a_model_call() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let held = profile
        .store
        .acquire_lease_for(
            s,
            LeaseKind::Turn,
            &Holder::current(),
            Duration::from_mins(1),
        )
        .unwrap();
    assert!(matches!(held, pmt_store::Acquire::Granted(_)));
    let model = mock_model(&profile, "[[rule]]\ntext = \"x\"\n", "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    let result = run.turn(&profile, "hi").await;

    assert_eq!(result.exit_reason, ExitReason::LeaseUnavailable);
    assert_eq!(
        result.error.as_ref().unwrap().code().code,
        "turn.lease_unavailable"
    );
    assert!(model.requests().is_empty(), "no model call");
    assert!(result.messages_appended.is_empty());
    assert_eq!(run.end_reasons(), ["lease_unavailable"]);
}

#[tokio::test]
async fn a_lost_lease_interrupts_the_turn() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, SLOW, "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    let db = profile.dir().join("state.db");
    let operator = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.busy_timeout(Duration::from_secs(5)).unwrap();
        conn.execute("DELETE FROM leases", []).unwrap();
    });
    let result = run.turn(&profile, "hi").await;
    operator.await.unwrap();

    assert_eq!(result.exit_reason, ExitReason::InterruptedBySystem);
    assert_eq!(result.interrupt_issuer.as_deref(), Some("lease_lost"));
    assert_eq!(run.end_reasons(), ["interrupted_by_system"]);
}

#[tokio::test]
async fn a_user_interrupt_ends_the_turn() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, SLOW, "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    let options = options();
    let interrupt = options.interrupt.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        interrupt.stop(Issuer::User);
    });
    let started = std::time::Instant::now();
    let result = run
        .turn_with(&profile, TurnInput::text("hi"), options)
        .await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(result.exit_reason, ExitReason::InterruptedByUser);
    assert_eq!(result.interrupt_issuer.as_deref(), Some("user"));
    assert_eq!(result.error.unwrap().code().code, "turn.interrupted");
}

#[tokio::test]
async fn the_run_budget_ends_the_turn() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, SLOW, "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    let mut options = options();
    options.run_budget = Some(Duration::from_millis(200));
    let result = run
        .turn_with(&profile, TurnInput::text("hi"), options)
        .await;
    assert_eq!(result.exit_reason, ExitReason::RunBudgetExhausted);
    assert_eq!(run.end_reasons(), ["run_budget_exhausted"]);
}

struct StopAfterOneRound;

impl Guardrail for StopAfterOneRound {
    fn after_tools(&self, _: &[RequestedCall], _: &[ToolOutcome]) -> Option<String> {
        Some("the same call repeated".to_owned())
    }
}

#[tokio::test]
async fn a_guardrail_stops_the_turn() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cron);
    let model = mock_model(&profile, TOOL_THEN_ANSWER, "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |setup| {
        setup.guardrail = Arc::new(StopAfterOneRound);
    });
    let result = run.turn(&profile, "go").await;
    assert_eq!(result.exit_reason, ExitReason::GuardrailStop);
    assert_eq!(result.notices, ["the same call repeated"]);
    assert_eq!(result.iterations, 1);
}

#[tokio::test]
async fn an_empty_answer_ends_with_empty_response() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, "[[rule]]\ntext = \"\"\n", "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    let result = run.turn(&profile, "hi").await;
    assert_eq!(result.exit_reason, ExitReason::EmptyResponse);
    assert_eq!(result.final_response, None);
    assert_eq!(result.error.unwrap().code().code, "turn.empty_response");
}

#[tokio::test]
async fn a_context_overflow_ends_with_context_exhausted() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, "[[rule]]\nfault = \"context_overflow\"\n", "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    let result = run.turn(&profile, "hi").await;
    assert_eq!(result.exit_reason, ExitReason::ContextExhausted);
}

#[tokio::test]
async fn an_auth_error_ends_with_provider_failed_at_once() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, "[[rule]]\nfault = \"auth_error\"\n", "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    let started = std::time::Instant::now();
    let result = run.turn(&profile, "hi").await;
    assert_eq!(result.exit_reason, ExitReason::ProviderFailed);
    assert_eq!(result.error.unwrap().code().code, "provider.auth_failed");
    assert_eq!(model.requests().len(), 1);
    assert!(started.elapsed() < Duration::from_secs(5), "no backoff");
}

#[test]
fn exit_reasons_are_a_closed_set() {
    for reason in ExitReason::ALL {
        let json = serde_json::to_string(&reason).unwrap();
        assert_eq!(json, format!("\"{}\"", reason.as_str()));
        assert_eq!(serde_json::from_str::<ExitReason>(&json).unwrap(), reason);
    }
    assert!(serde_json::from_str::<ExitReason>("\"max_iterations\"").is_err());
    assert!(serde_json::from_str::<ExitReason>("\"interrupted\"").is_err());
    assert_eq!(ExitReason::Crashed.as_str(), "crashed");
}
