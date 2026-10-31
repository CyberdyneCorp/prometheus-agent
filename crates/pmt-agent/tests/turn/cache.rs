//! Cache-hit reporting and `cache.unexpected_break` detection (spec:
//! agent-turn-loop, Cache-hit reporting and break detection; Step events
//! per iteration).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use pmt_agent::turn::events::EventSink;
use pmt_agent::turn::model::BoxFuture;
use pmt_agent::turn::{ExitReason, Recorded};
use pmt_core::id::SessionId;
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::CacheBreak;
use pmt_obs::trajectory::writer::Emit;
use pmt_store::{CostStatus, Source, Tokens, UsageEvent};

use crate::support::{FakeTools, Profile, agent, mock_model};

const TWO_ROUNDS: &str = r#"
[[rule]]
times = 2
[[rule.tool_calls]]
name = "lookup"
arguments = { q = "x" }

[[rule]]
text = "done"
"#;

fn breaks(events: &Recorded) -> Vec<CacheBreak> {
    events
        .events()
        .into_iter()
        .filter_map(|e| match e {
            Event::CacheUnexpectedBreak(b) => Some(b),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn an_append_only_session_never_breaks_the_cache() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, TWO_ROUNDS, "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    run.turn(&profile, "one").await;
    run.turn(&profile, "two").await;
    assert!(breaks(&run.events).is_empty());
}

#[tokio::test]
async fn an_edited_earlier_tool_result_is_detected_with_its_index() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, TWO_ROUNDS, "mock").await;
    let db = profile.dir().join("state.db");
    let batches = Arc::new(AtomicU32::new(0));
    let seen = batches.clone();
    let tools = Arc::new(FakeTools {
        // A bug: the second batch rewrites the first tool result.
        effect: Some(Box::new(move || {
            if seen.fetch_add(1, Ordering::SeqCst) == 1 {
                let conn = rusqlite::Connection::open(&db).unwrap();
                conn.execute(
                    "UPDATE messages SET content = 'tampered' WHERE role = 'tool'",
                    [],
                )
                .unwrap();
            }
        })),
        ..FakeTools::plain()
    });
    let run = agent(&profile, s, model, tools, |_| {});
    let result = run.turn(&profile, "go").await;
    assert_eq!(result.exit_reason, ExitReason::Completed);

    let found = breaks(&run.events);
    assert_eq!(found.len(), 1, "{found:?}");
    // user (0), assistant (1), the edited tool result (2).
    assert_eq!(found[0].first_diff_index, Some(2));
    assert_ne!(found[0].expected_prefix_hash, found[0].actual_prefix_hash);
}

#[tokio::test]
async fn a_recorded_prompt_rebuild_is_not_a_break() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, "[[rule]]\ntext = \"ok\"\nrepeat = true\n", "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    run.turn(&profile, "one").await;
    run.agent.apply_personality_now("concise");
    run.turn(&profile, "two").await;
    let kinds = run.kinds();
    assert_eq!(kinds.iter().filter(|k| **k == "prompt.rebuilt").count(), 2);
    assert!(breaks(&run.events).is_empty());
}

#[tokio::test]
async fn turn_end_reports_the_session_cache_hit_rate() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    record(&profile, s, 10_000, 90_000);
    let model = mock_model(&profile, "[[rule]]\ntext = \"ok\"\n", "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    run.turn(&profile, "hi").await;
    let rate = run
        .events
        .events()
        .into_iter()
        .find_map(|e| match e {
            Event::TurnEnd(end) => end.cache_hit_rate,
            _ => None,
        })
        .unwrap();
    // 90,000 cached of a little over 100,000 input tokens (the mock call
    // adds its own uncached input).
    assert!((0.89..0.9).contains(&rate), "{rate}");
}

/// Writes a ledger row of `input` uncached and `cached` cache-read tokens.
fn record(profile: &Profile, session: SessionId, input: u64, cached: u64) {
    profile
        .store
        .record_usage(&UsageEvent {
            call_id: format!("call-{input}-{cached}"),
            ts: jiff::Timestamp::now(),
            session_id: session,
            turn_id: None,
            parent_call_id: None,
            purpose: "main".into(),
            purpose_detail: None,
            provider: "p".into(),
            model: "m".into(),
            service_tier: None,
            tokens: Tokens {
                input,
                output: 10,
                cache_read: cached,
                cache_write: 0,
                reasoning: 0,
            },
            cost_usd: None,
            cost_status: CostStatus::Unknown,
            billing_source: "api_key:p".into(),
            request_hash: None,
            cache_hit: false,
        })
        .unwrap();
}

/// Fails on every `turn.step`.
struct FailingStep(Arc<Recorded>);

impl EventSink for FailingStep {
    fn emit(&self, emit: Emit) -> BoxFuture<'_, Result<(), pmt_core::Error>> {
        let step = matches!(emit.event, Event::TurnStep(_));
        let recorded = self.0.emit(emit);
        Box::pin(async move {
            recorded.await?;
            if step {
                Err(pmt_core::Error::new(
                    &pmt_core::error::codes::PROVIDER_CALL_FAILED,
                    "plugin subscriber broke",
                ))
            } else {
                Ok(())
            }
        })
    }
}

#[tokio::test]
async fn a_failing_step_subscriber_does_not_abort_the_turn() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, TWO_ROUNDS, "mock").await;
    let recorded = Recorded::new();
    let run = agent(&profile, s, model, FakeTools::new(), |setup| {
        setup.events = Arc::new(FailingStep(recorded.clone()));
    });
    let result = run.turn(&profile, "go").await;
    assert_eq!(result.exit_reason, ExitReason::Completed);
    let steps = recorded
        .events()
        .into_iter()
        .filter(|e| matches!(e, Event::TurnStep(_)))
        .count();
    assert_eq!(steps, 3, "one per iteration");
}
