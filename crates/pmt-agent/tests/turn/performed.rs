//! Actions an agent runtime performed itself (spec: model-providers, Codex
//! app-server runtime): projected into the transcript and trajectory as a
//! finished tool round, never run again, and counted as iterations so the
//! skill nudge sees them.

use std::sync::Arc;

use pmt_agent::turn::model::{
    BoxFuture, DeltaSink, ModelFailure, ModelPort, ModelReply, PerformedCall, RequestedCall,
    RouteInfo,
};
use pmt_obs::trajectory::events::Event;
use pmt_providers::cost::CallContext;
use pmt_providers::request::Request;
use pmt_store::{Role, Source, Visibility};

use crate::support::{FakeTools, Profile, Recording, agent, mock_model};

/// The recording model's route, answering with Codex's work and reply.
struct Runtime {
    inner: Arc<Recording>,
}

impl ModelPort for Runtime {
    fn route(&self) -> BoxFuture<'_, RouteInfo> {
        self.inner.route()
    }

    fn start_turn(&self) -> BoxFuture<'_, ()> {
        self.inner.start_turn()
    }

    fn call<'a>(
        &'a self,
        _request: &'a Request,
        _ctx: &'a CallContext,
        _deltas: &'a dyn DeltaSink,
    ) -> BoxFuture<'a, Result<ModelReply, ModelFailure>> {
        let performed = |id: &str, name: &str, arguments: &str, output: &str, ok| PerformedCall {
            call: RequestedCall {
                id: id.into(),
                name: name.into(),
                arguments: arguments.into(),
            },
            output: output.into(),
            ok,
        };
        Box::pin(async move {
            Ok(ModelReply {
                text: "Done: notes.txt created.".into(),
                performed: vec![
                    performed("cmd-1", "terminal", r#"{"command":"ls"}"#, "a\nb\n", true),
                    performed(
                        "chg-1",
                        "patch",
                        r#"{"paths":["notes.txt"]}"#,
                        "+hello",
                        true,
                    ),
                ],
                ..ModelReply::default()
            })
        })
    }
}

#[tokio::test]
async fn performed_actions_are_recorded_as_a_finished_tool_round() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let inner = mock_model(&profile, "[[rule]]\ntext = \"unused\"\n", "mock").await;
    let tools = FakeTools::new();
    let run = agent(
        &profile,
        s,
        Arc::new(Runtime { inner }),
        tools.clone(),
        |_| {},
    );
    let result = run.turn(&profile, "make notes").await;
    assert_eq!(
        result.final_response.as_deref(),
        Some("Done: notes.txt created.")
    );
    // Nothing ran again; each action counts as an iteration.
    assert_eq!(tools.calls(), 0);
    assert_eq!(result.iterations, 3);
    let rows = profile.store.messages(s, Visibility::Active).unwrap();
    let roles: Vec<Role> = rows.iter().map(|r| r.role).collect();
    assert_eq!(
        roles,
        [
            Role::User,
            Role::Assistant,
            Role::Tool,
            Role::Tool,
            Role::Assistant
        ]
    );
    let calls = rows[1].tool_calls.clone().unwrap();
    assert_eq!(calls[0]["name"], "terminal");
    assert_eq!(calls[1]["name"], "patch");
    assert_eq!(rows[2].tool_call_id.as_deref(), Some("cmd-1"));
    assert_eq!(rows[2].content.as_deref(), Some("a\nb\n"));
    let events = run.events.events();
    let called: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::ToolCall(call) => Some(call.tool.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(called, ["terminal", "patch"]);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::ToolResult(r) if r.ok))
            .count(),
        2
    );
}
