//! Mixture of Agents in the turn loop (spec: mixture-of-agents, MoA call
//! procedure; MoA preserves the session prompt cache).

use std::sync::{Arc, Mutex, PoisonError};

use pmt_agent::turn::model::{
    BoxFuture, DeltaSink, ModelFailure, ModelPort, ModelReply, RouteInfo,
};
use pmt_providers::cost::CallContext;
use pmt_providers::request::Request;
use pmt_store::Source;

use crate::support::{FakeTools, Priced, Profile, agent, scripted_port};

/// A `default` preset whose references and aggregator are mock models.
const PRESET: &str = "[moa.presets.default]\n\
    reference_models = [{provider = \"mock\", model = \"ref-a\"}, {provider = \"mock\", model = \"ref-b\"}]\n\
    aggregator = {provider = \"mock\", model = \"agg\"}\n";

/// The references advise; the aggregator calls `lookup`, then answers.
const SCRIPT: &str = r#"
[[rule]]
model = "ref-a"
repeat = true
text = "advice A"

[[rule]]
model = "ref-b"
repeat = true
text = "advice B"

[[rule]]
model = "agg"
step = 0
[[rule.tool_calls]]
name = "lookup"
arguments = { q = "x" }

[[rule]]
model = "agg"
repeat = true
text = "Done."

[[rule]]
model = "plain"
repeat = true
text = "plain answer"
"#;

/// Scenario: Aggregator tool call executed.
#[tokio::test]
async fn the_aggregators_tool_call_runs_and_the_next_iteration_follows_the_fanout() {
    let profile = Profile::new(PRESET);
    let session = profile.session(Source::Cli);
    let model = scripted_port(&profile, SCRIPT, ("moa", "default"), Priced::Catalog).await;
    let tools = Arc::new(FakeTools::plain());
    let run = agent(&profile, session, model.clone(), tools.clone(), |_| {});
    let result = run.turn(&profile, "work").await;
    assert_eq!(result.final_response.as_deref(), Some("Done."));
    let calls = tools
        .calls
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "lookup");
    // `user_turn`: the references ran once, the aggregator twice.
    let mut purposes: Vec<String> = profile
        .store
        .usage_events(session)
        .unwrap()
        .into_iter()
        .map(|e| format!("{} {}", e.purpose, e.model))
        .collect();
    purposes.sort();
    assert_eq!(
        purposes,
        [
            "moa.aggregator agg",
            "moa.aggregator agg",
            "moa.reference ref-a",
            "moa.reference ref-b",
        ]
    );
    assert!(
        result
            .notices
            .iter()
            .any(|n| n == "advisor mock/ref-a:\nadvice A"),
        "{:?}",
        result.notices
    );
    // The session's own history never keeps the guidance.
    let second = &model.requests()[1];
    assert!(
        second
            .messages
            .iter()
            .flat_map(|m| &m.parts)
            .all(|p| !format!("{p:?}").contains("moa-guidance"))
    );
}

/// A port whose route is switched between turns.
struct Switched(Mutex<Arc<dyn ModelPort>>);

impl Switched {
    fn port(&self) -> Arc<dyn ModelPort> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set(&self, port: Arc<dyn ModelPort>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = port;
    }
}

impl ModelPort for Switched {
    fn route(&self) -> BoxFuture<'_, RouteInfo> {
        Box::pin(async move { self.port().route().await })
    }

    fn call<'a>(
        &'a self,
        request: &'a Request,
        ctx: &'a CallContext,
        deltas: &'a dyn DeltaSink,
    ) -> BoxFuture<'a, Result<ModelReply, ModelFailure>> {
        Box::pin(async move { self.port().call(request, ctx, deltas).await })
    }
}

fn prefix(request: &Request) -> (Vec<u8>, Vec<u8>) {
    (
        serde_json::to_vec(&request.system).unwrap(),
        serde_json::to_vec(&request.tools).unwrap(),
    )
}

/// Scenario: Returning from MoA replays the prefix.
#[tokio::test]
async fn returning_from_a_preset_replays_the_same_system_prompt_and_tools() {
    let profile = Profile::new(PRESET);
    let session = profile.session(Source::Cli);
    let plain = scripted_port(&profile, SCRIPT, ("mock", "plain"), Priced::Catalog).await;
    let moa = scripted_port(&profile, SCRIPT, ("moa", "default"), Priced::Catalog).await;
    let port = Arc::new(Switched(Mutex::new(plain.clone())));
    let tools = Arc::new(FakeTools::plain());
    let run = agent(&profile, session, port.clone(), tools, |_| {});
    run.turn(&profile, "turn one").await;
    port.set(moa.clone());
    let second = run.turn(&profile, "turn two").await;
    assert_eq!(second.final_response.as_deref(), Some("Done."));
    port.set(plain.clone());
    run.turn(&profile, "turn three").await;
    let sent = plain.requests();
    assert_eq!(sent.len(), 2);
    assert_eq!(prefix(&sent[0]), prefix(&sent[1]), "system and tool bytes");
    assert_eq!(
        prefix(&moa.requests()[0]),
        prefix(&sent[0]),
        "MoA left them alone"
    );
}
