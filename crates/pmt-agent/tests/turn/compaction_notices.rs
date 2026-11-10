//! Idle compaction, compaction lifecycle events with the context pressure
//! of each turn, and server-side Responses compaction (spec:
//! context-compression, Idle compaction; Compaction progress notices and
//! context pressure gauge; Server-side Responses compaction (opt-in)), on
//! golden transcripts over the mock provider.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pmt_agent::turn::model::{
    BoxFuture, DeltaSink, ModelFailure, ModelPort, ModelReply, RouteInfo,
};
use pmt_config::schema::WireFormat;
use pmt_core::context_pressure::{Level, of_turn_end};
use pmt_obs::trajectory::events::Event;
use pmt_providers::cost::CallContext;
use pmt_providers::request::{Part, Request};
use pmt_store::{Source, Visibility};
use serde_json::json;

use crate::compression::{FakeSummarizer, SMALL, committed, exchanges, with_summarizer};
use crate::support::{FakeTools, Profile, Recording, agent, mock_model};

const OK: &str = "[[rule]]\ntext = \"ok\"\nrepeat = true\n";

fn idle_profile(extra: &str) -> Profile {
    Profile::new(&format!(
        "{SMALL}[compression]\nidle_compact_after_secs = 1\n{extra}"
    ))
}

/// The kinds of `run`'s events, in order.
fn kinds(events: &[Event]) -> Vec<&'static str> {
    events.iter().map(Event::kind).collect()
}

/// Scenario: Return after a long pause (Idle compaction).
#[tokio::test]
async fn a_turn_after_a_long_pause_compacts_with_trigger_idle_before_its_model_call() {
    let profile = idle_profile("");
    let s = profile.session(Source::Gateway);
    // About 6,400 tokens: 40% of the 16K window, under the 13,600-token
    // trigger, over the 2,720-token idle floor.
    exchanges(&profile, s, 12, 2_000);
    let model = mock_model(&profile, OK, "mock").await;
    let summarizer = FakeSummarizer::new();
    let run = agent(
        &profile,
        s,
        model.clone(),
        FakeTools::new(),
        with_summarizer(&summarizer),
    );
    tokio::time::sleep(Duration::from_millis(1_100)).await;

    run.turn(&profile, "back again").await;

    let done = committed(&run);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].trigger, "idle");
    assert!(done[0].tokens_after < done[0].tokens_before);
    let events = run.events.events();
    let order = kinds(&events);
    let started = order.iter().position(|k| *k == "compression.started");
    let commit = order.iter().position(|k| *k == "compression.committed");
    let call = order.iter().position(|k| *k == "model.call");
    assert!(
        started < commit && commit < call,
        "started, committed, then the model call: {order:?}"
    );
    // The turn's only main request already went out compacted.
    assert_eq!(model.requests().len(), 1);
}

/// Scenario: Small context skipped (Idle compaction).
#[tokio::test]
async fn a_small_context_is_not_compacted_after_a_pause() {
    let profile = idle_profile("");
    let s = profile.session(Source::Gateway);
    // About 800 tokens: 5% of the window.
    exchanges(&profile, s, 4, 700);
    let model = mock_model(&profile, OK, "mock").await;
    let summarizer = FakeSummarizer::new();
    let run = agent(
        &profile,
        s,
        model,
        FakeTools::new(),
        with_summarizer(&summarizer),
    );
    tokio::time::sleep(Duration::from_millis(1_100)).await;

    run.turn(&profile, "back again").await;

    assert!(committed(&run).is_empty());
    assert!(!kinds(&run.events.events()).contains(&"compression.started"));
}

#[tokio::test]
async fn idle_compaction_is_off_by_default() {
    let profile = Profile::new(SMALL);
    let s = profile.session(Source::Gateway);
    exchanges(&profile, s, 12, 2_000);
    let model = mock_model(&profile, OK, "mock").await;
    let summarizer = FakeSummarizer::new();
    let run = agent(
        &profile,
        s,
        model,
        FakeTools::new(),
        with_summarizer(&summarizer),
    );
    tokio::time::sleep(Duration::from_millis(1_100)).await;

    run.turn(&profile, "back again").await;

    assert!(committed(&run).is_empty());
}

fn request_text(request: &Request) -> String {
    let mut text = crate::support::system_text(request);
    for message in &request.messages {
        for part in &message.parts {
            if let Part::Text { text: t } = part {
                text.push_str(t);
            }
        }
    }
    text
}

/// Scenario: Gauge crossing: the turn's end reports the context against
/// the compaction trigger (what the CLI draws its gauge from), and the
/// next request carries no gauge text.
#[tokio::test]
async fn each_turn_reports_its_context_pressure_and_no_request_carries_it() {
    let profile = Profile::new(SMALL);
    let s = profile.session(Source::Cli);
    // About 8,800 tokens of transcript: past 60% of the 13,600 trigger.
    exchanges(&profile, s, 16, 2_200);
    let model = mock_model(&profile, OK, "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});

    run.turn(&profile, "how close are we").await;
    run.turn(&profile, "and now").await;

    let ends: Vec<serde_json::Value> = run
        .events
        .events()
        .iter()
        .filter(|e| e.kind() == "turn.end")
        .map(Event::payload)
        .collect();
    let (percent, level) = of_turn_end(&ends[0]).expect("context and trigger reported");
    assert!(
        level >= Some(Level::Info),
        "{percent}% of the trigger: {:?}",
        ends[0]
    );
    for request in model.requests() {
        let text = request_text(&request);
        assert!(!text.contains("to compaction"), "gauge text sent");
        assert!(!text.contains('█'), "gauge bar sent");
    }
}

/// A Responses route that compacts server-side: its first reply carries
/// an opaque compaction item; every request is recorded.
struct ServerCompacting {
    inner: Arc<Recording>,
    supported: bool,
    replies: Mutex<u32>,
}

const ITEM: &str = "enc_ABC123";

impl ModelPort for ServerCompacting {
    fn route(&self) -> BoxFuture<'_, RouteInfo> {
        Box::pin(async move {
            let mut info = self.inner.route().await;
            info.wire_format = WireFormat::Responses;
            info.server_compaction = self.supported;
            info
        })
    }

    fn start_turn(&self) -> BoxFuture<'_, ()> {
        self.inner.start_turn()
    }

    fn call<'a>(
        &'a self,
        request: &'a Request,
        ctx: &'a CallContext,
        deltas: &'a dyn DeltaSink,
    ) -> BoxFuture<'a, Result<ModelReply, ModelFailure>> {
        Box::pin(async move {
            let mut reply = self.inner.call(request, ctx, deltas).await?;
            let mut n = self.replies.lock().unwrap_or_else(PoisonError::into_inner);
            if *n == 0 {
                reply.compaction_item = Some(json!({
                    "type": "compaction", "id": "cmp_1", "encrypted_content": ITEM
                }));
            }
            *n += 1;
            Ok(reply)
        })
    }
}

async fn server_model(
    profile: &Profile,
    supported: bool,
) -> (Arc<ServerCompacting>, Arc<Recording>) {
    let inner = mock_model(profile, OK, "mock").await;
    let model = Arc::new(ServerCompacting {
        inner: inner.clone(),
        supported,
        replies: Mutex::new(0),
    });
    (model, inner)
}

fn texts_of(request: &Request) -> Vec<String> {
    request
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            Part::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// Scenario: Compaction item replayed.
#[tokio::test]
async fn the_stored_compaction_item_replaces_the_history_it_covers() {
    let profile = Profile::new(&format!(
        "{SMALL}[compression]\nresponses_native = true\nresponses_compact_threshold_tokens = 5000\n"
    ));
    let s = profile.session(Source::Cli);
    let (model, recorded) = server_model(&profile, true).await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});

    run.turn(&profile, "first question").await;
    run.turn(&profile, "second question").await;

    let requests = recorded.requests();
    let first = requests[0].server_compaction.as_ref().expect("sent");
    assert_eq!(first.threshold, 5000);
    assert!(first.item.is_none());
    let second = requests[1].server_compaction.as_ref().expect("sent");
    assert_eq!(
        second.item.as_ref().unwrap()["encrypted_content"],
        ITEM,
        "the stored item is replayed"
    );
    let sent = texts_of(&requests[1]);
    assert!(
        !sent.iter().any(|t| t.contains("first question")),
        "{sent:?}"
    );
    assert!(
        sent.iter().any(|t| t.contains("second question")),
        "{sent:?}"
    );
    // The transcript keeps every message.
    let stored: Vec<String> = profile
        .store
        .messages(s, Visibility::All)
        .unwrap()
        .iter()
        .filter_map(|m| m.content.clone())
        .collect();
    assert!(stored.iter().any(|t| t == "first question"), "{stored:?}");
    assert!(stored.iter().any(|t| t == "second question"), "{stored:?}");
}

#[tokio::test]
async fn a_route_without_server_compaction_ignores_the_setting() {
    let profile = Profile::new(&format!("{SMALL}[compression]\nresponses_native = true\n"));
    let s = profile.session(Source::Cli);
    let (model, recorded) = server_model(&profile, false).await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});

    run.turn(&profile, "first question").await;
    run.turn(&profile, "second question").await;

    for request in recorded.requests() {
        assert!(request.server_compaction.is_none());
    }
    let sent = texts_of(&recorded.requests()[1]);
    assert!(
        sent.iter().any(|t| t.contains("first question")),
        "full history"
    );
}
