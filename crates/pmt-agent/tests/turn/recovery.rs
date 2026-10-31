//! Transcript repair, model-call retries and auto-recovery, and the
//! failed-turn boundary row, through fault-injecting mock scripts (spec:
//! agent-turn-loop, Transcript repair before each request; Model call
//! retries and auto-recovery; Failed-turn boundary row).

use std::sync::Arc;
use std::time::Duration;

use pmt_agent::turn::ExitReason;
use pmt_agent::turn::model::{
    BoxFuture, DeltaSink, ModelFailure, ModelPort, ModelReply, RouteInfo,
};
use pmt_agent::turn::repair::{MALFORMED_ARGUMENTS, MISSING_RESULT};
use pmt_config::schema::WireFormat;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_providers::classify::ErrorClass;
use pmt_providers::cost::CallContext;
use pmt_providers::request::{Part, Request, Role as W};
use pmt_store::{NewMessage, Role, Source, Visibility};
use serde_json::json;

use crate::support::{FakeTools, Profile, agent, mock_model};

fn stored(profile: &Profile, s: SessionId) -> Vec<(Role, String)> {
    profile
        .store
        .messages(s, Visibility::Active)
        .unwrap()
        .into_iter()
        .map(|m| (m.role, m.content.unwrap_or_default()))
        .collect()
}

#[tokio::test]
async fn consecutive_user_rows_are_merged_on_the_wire_only() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    // A crash left the history ending `user, user`.
    for text in ["first", "second"] {
        profile
            .store
            .append_message(s, &NewMessage::new(Role::User, text))
            .unwrap();
    }
    let model = mock_model(&profile, "[[rule]]\ntext = \"ok\"\n", "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    run.turn(&profile, "third").await;

    let request = &model.requests()[0];
    assert_eq!(request.messages.len(), 1, "one merged user row");
    assert_eq!(
        request.messages[0].parts,
        [
            Part::text("first"),
            Part::text("second"),
            Part::text("third")
        ]
    );
    let roles: Vec<Role> = stored(&profile, s).into_iter().map(|(r, _)| r).collect();
    assert_eq!(
        roles,
        [Role::User, Role::User, Role::User, Role::Assistant],
        "stored rows unchanged"
    );
}

#[tokio::test]
async fn orphan_calls_results_and_malformed_arguments_are_repaired() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let store = &profile.store;
    store
        .append_message(s, &NewMessage::new(Role::User, "go"))
        .unwrap();
    let mut assistant = NewMessage::new(Role::Assistant, "");
    assistant.tool_calls = Some(json!([
        {"id": "c1", "name": "lookup", "arguments": "{\"q\": \"a\"}"},
        {"id": "c2", "name": "lookup", "arguments": "{\"q\": "}
    ]));
    store.append_message(s, &assistant).unwrap();
    let mut answered = NewMessage::new(Role::Tool, "a result");
    answered.tool_call_id = Some("c1".into());
    store.append_message(s, &answered).unwrap();
    let mut stray = NewMessage::new(Role::Tool, "nobody asked");
    stray.tool_call_id = Some("zz".into());
    store.append_message(s, &stray).unwrap();

    let model = mock_model(&profile, "[[rule]]\ntext = \"ok\"\n", "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    run.turn(&profile, "again").await;

    let request = &model.requests()[0];
    let roles: Vec<W> = request.messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, [W::User, W::Assistant, W::Tool, W::Tool, W::User]);
    assert!(matches!(
        &request.messages[1].parts[1],
        Part::ToolCall { id, arguments, .. } if id == "c2" && arguments == MALFORMED_ARGUMENTS
    ));
    assert!(matches!(
        &request.messages[3].parts[0],
        Part::ToolResult { call_id, content, is_error: true } if call_id == "c2" && content == MISSING_RESULT
    ));
    let text = format!("{:?}", request.messages);
    assert!(
        !text.contains("nobody asked"),
        "the stray result is dropped"
    );
    assert_eq!(
        stored(&profile, s)[3].1,
        "nobody asked",
        "stored rows unchanged"
    );
}

#[tokio::test]
async fn a_single_configured_attempt_is_not_retried() {
    let script = "[[rule]]\nfault = \"server_error\"\nstatus = 503\n\n[[rule]]\ntext = \"ok\"\n";
    let profile = Profile::new("[agent]\napi_max_retries = 1\nauto_recovery_cycles = 0\n");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, script, "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    assert_eq!(
        run.turn(&profile, "hi").await.exit_reason,
        ExitReason::ProviderFailed
    );

    // A second attempt on the same route recovers.
    let profile = Profile::new("[agent]\napi_max_retries = 2\nauto_recovery_cycles = 0\n");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, script, "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    assert_eq!(
        run.turn(&profile, "hi").await.exit_reason,
        ExitReason::Completed
    );
}

#[tokio::test(start_paused = true)]
async fn a_transient_outage_is_waited_out() {
    // Three attempts fail (api_max_retries = 3), then the provider is back.
    let script = "[[rule]]\nfault = \"server_error\"\ntimes = 3\n\n[[rule]]\ntext = \"back\"\n";
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, script, "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    let started = tokio::time::Instant::now();
    let result = run.turn(&profile, "hi").await;

    assert_eq!(result.exit_reason, ExitReason::Completed);
    assert_eq!(result.final_response.as_deref(), Some("back"));
    assert_eq!(model.requests().len(), 2, "one recovery cycle");
    assert!(
        started.elapsed() >= Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn recovery_cycles_end_with_provider_failed_and_a_boundary_row() {
    let script = "[[rule]]\nfault = \"timeout\"\nrepeat = true\n";
    let profile = Profile::new("[agent]\nauto_recovery_cycles = 2\n");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, script, "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    let started = tokio::time::Instant::now();
    let result = run.turn(&profile, "hi").await;

    assert_eq!(result.exit_reason, ExitReason::ProviderFailed);
    assert_eq!(
        model.requests().len(),
        3,
        "the call and two recovery cycles"
    );
    assert!(started.elapsed() >= Duration::from_secs(45));
    let rows = profile.store.messages(s, Visibility::Active).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].role, Role::Assistant);
    assert_eq!(
        rows[1].metadata.as_ref().unwrap()["prometheus_authored"],
        true
    );
}

#[tokio::test]
async fn non_retryable_failures_skip_recovery() {
    for (fault, reason, rows) in [
        ("auth_error", ExitReason::ProviderFailed, 2),
        ("context_overflow", ExitReason::ContextExhausted, 1),
    ] {
        let profile = Profile::new("");
        let s = profile.session(Source::Cli);
        let model = mock_model(
            &profile,
            &format!("[[rule]]\nfault = \"{fault}\"\n"),
            "mock",
        )
        .await;
        let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
        let result = run.turn(&profile, "hi").await;
        assert_eq!(result.exit_reason, reason, "{fault}");
        assert_eq!(model.requests().len(), 1, "{fault}");
        // An overflow gets no boundary row (compaction handles it).
        assert_eq!(stored(&profile, s).len(), rows, "{fault}");
    }
}

/// A model whose every call fails with `message`.
struct Failing(String);

impl ModelPort for Failing {
    fn route(&self) -> BoxFuture<'_, RouteInfo> {
        Box::pin(async {
            RouteInfo {
                provider: "fake".into(),
                model: "fake".into(),
                price: None,
                context_window: None,
                max_output_tokens: None,
                wire_format: WireFormat::ChatCompletions,
            }
        })
    }

    fn call<'a>(
        &'a self,
        _: &'a Request,
        _: &'a CallContext,
        _: &'a dyn DeltaSink,
    ) -> BoxFuture<'a, Result<ModelReply, ModelFailure>> {
        let error = Error::new(&codes::PROVIDER_INVALID_REQUEST, self.0.clone());
        Box::pin(async move { Err(ModelFailure::new(error, Some(ErrorClass::InvalidRequest))) })
    }
}

#[tokio::test]
async fn the_boundary_row_is_redacted_and_capped() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let secret = "sk-abcdefghijklmnopqrstuvwxyz123456";
    let message = format!("bad request with key {secret} {}", "x".repeat(5_000));
    let run = agent(
        &profile,
        s,
        Arc::new(Failing(message)),
        FakeTools::new(),
        |_| {},
    );
    let result = run.turn(&profile, "hi").await;
    assert_eq!(result.exit_reason, ExitReason::ProviderFailed);
    let rows = stored(&profile, s);
    assert_eq!(rows.len(), 2);
    let boundary = &rows[1].1;
    assert!(
        boundary.starts_with("[prometheus] The turn ended (provider_failed)"),
        "{boundary}"
    );
    assert!(!boundary.contains(secret));
    assert!(boundary.chars().count() <= 2_000);

    // The next user message is a new row after the boundary, never merged.
    run.turn(&profile, "retry").await;
    let roles: Vec<Role> = stored(&profile, s).into_iter().map(|(r, _)| r).collect();
    assert_eq!(
        roles,
        [Role::User, Role::Assistant, Role::User, Role::Assistant]
    );
}
