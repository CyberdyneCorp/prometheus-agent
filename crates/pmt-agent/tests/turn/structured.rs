//! Structured steering and the budget checkpoint notice (spec:
//! agent-turn-loop, Structured steering; Budget checkpoint notice).

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pmt_agent::turn::{ExitReason, SteerKind};
use pmt_providers::request::{Part, Role as W};
use pmt_store::{Role, Source, Visibility};

use crate::support::{FakeTools, Profile, agent, mock_model};

/// A tool call on every step up to `calls`, then an answer.
fn tool_steps(calls: u32) -> String {
    let mut script = String::new();
    for step in 0..calls {
        let _ = write!(
            script,
            "[[rule]]\nstep = {step}\n[[rule.tool_calls]]\nname = \"lookup\"\narguments = {{ q = \"{step}\" }}\n\n"
        );
    }
    let _ = write!(script, "[[rule]]\nstep = {calls}\ntext = \"Final.\"\n");
    script
}

fn slow_tools(delay: Duration) -> Arc<FakeTools> {
    Arc::new(FakeTools {
        delay,
        ..FakeTools::plain()
    })
}

#[tokio::test]
async fn a_wrap_up_steer_ends_the_turn_with_one_toolless_request() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    // The model would keep calling tools; the wrap-up request offers none.
    let model = mock_model(&profile, &tool_steps(1), "mock").await;
    let tools = slow_tools(Duration::from_millis(200));
    let run = agent(&profile, s, model.clone(), tools.clone(), |_| {});
    let steer = async {
        tools.started.notified().await;
        run.agent
            .steer_with("finish up now", SteerKind::WrapUp)
            .unwrap();
    };
    let (result, ()) = tokio::join!(run.turn(&profile, "work"), steer);

    assert_eq!(result.exit_reason, ExitReason::Completed);
    assert_eq!(tools.calls(), 1, "the running batch finished, no other ran");
    let requests = model.requests();
    assert_eq!(requests.len(), 2, "exactly one more request");
    let last = &requests[1];
    assert!(last.tools.is_empty(), "the wrap-up request is toolless");
    let roles: Vec<_> = last.messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, [W::User, W::Assistant, W::Tool, W::User]);
    assert_eq!(
        last.messages[3].parts,
        [Part::text(
            "[OUT-OF-BAND USER MESSAGE kind=wrap_up]\nfinish up now\n[/OUT-OF-BAND USER MESSAGE]"
        )]
    );
    assert_eq!(result.final_response.as_deref(), Some("Final."));
}

#[tokio::test]
async fn a_redirect_makes_running_tools_yield_and_is_a_real_user_message() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, &tool_steps(1), "mock").await;
    let tools = slow_tools(Duration::from_secs(30));
    let run = agent(&profile, s, model.clone(), tools.clone(), |_| {});
    let started = Instant::now();
    let steer = async {
        tools.started.notified().await;
        run.agent
            .steer_with("stop and check the logs instead", SteerKind::Redirect)
            .unwrap();
    };
    let (result, ()) = tokio::join!(run.turn(&profile, "work"), steer);

    assert_eq!(result.exit_reason, ExitReason::Completed);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the batch yielded"
    );
    let second = &model.requests()[1];
    let roles: Vec<_> = second.messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, [W::User, W::Assistant, W::Tool, W::User]);
    assert_eq!(
        second.messages[3].parts,
        [Part::text(
            "[OUT-OF-BAND USER MESSAGE kind=redirect]\nstop and check the logs instead\n[/OUT-OF-BAND USER MESSAGE]"
        )]
    );
    let stored = profile.store.messages(s, Visibility::Active).unwrap();
    let redirect = stored
        .iter()
        .find(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.contains("kind=redirect"))
        })
        .unwrap();
    assert_eq!(redirect.role, Role::User);
    let metadata = redirect.metadata.clone().unwrap();
    assert!(
        metadata.get("prometheus_authored").is_none(),
        "a real user message, not a notice: {metadata}"
    );
}

#[tokio::test]
async fn the_budget_checkpoint_rides_the_newest_unsent_tool_result_once() {
    let profile = Profile::new("[agent]\nmax_iterations = 20\nbudget_warning_ratio = 0.75\n");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, &tool_steps(17), "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    let result = run.turn(&profile, "work").await;
    assert_eq!(result.exit_reason, ExitReason::Completed);

    let last = model.requests().pop().unwrap();
    let results: Vec<String> = last
        .messages
        .iter()
        .filter(|m| m.role == W::Tool)
        .map(|m| match &m.parts[0] {
            Part::ToolResult { content, .. } => content.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(results.len(), 17);
    let marked: Vec<usize> = results
        .iter()
        .enumerate()
        .filter(|(_, r)| r.contains("[budget checkpoint]"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(marked, [14], "the 15th iteration's result, once");
    assert!(
        results[14].ends_with("[budget checkpoint] used 15 of 20 iterations"),
        "{}",
        results[14]
    );
    assert!(
        last.system
            .iter()
            .all(|b| !b.text.contains("budget checkpoint")),
        "never in the system prompt"
    );
}

#[tokio::test]
async fn no_checkpoint_without_the_ratio_or_for_unlimited_budgets() {
    for config in [
        "[agent]\nmax_iterations = 20\n",
        "[agent]\nmax_iterations = 0\nbudget_warning_ratio = 0.5\n",
    ] {
        let profile = Profile::new(config);
        let s = profile.session(Source::Cli);
        let model = mock_model(&profile, &tool_steps(16), "mock").await;
        let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
        run.turn(&profile, "work").await;
        let last = model.requests().pop().unwrap();
        let text = format!("{:?}", last.messages);
        assert!(!text.contains("budget checkpoint"), "{config}");
    }
}
