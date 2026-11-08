//! Fast mode per request (spec: model-providers, Fast mode service tier):
//! which requests of a turn ask for the priority tier.

use std::sync::Arc;
use std::time::Duration;

use pmt_agent::turn::fast::NO_TIER_NOTICE;
use pmt_agent::turn::model::{
    BoxFuture, DeltaSink, ModelFailure, ModelPort, ModelReply, RouteInfo,
};
use pmt_config::schema::FastMode;
use pmt_providers::cost::CallContext;
use pmt_providers::fast::Speed;
use pmt_providers::request::Request;
use pmt_store::Source;

use crate::support::{FakeTools, Profile, Recording, agent, mock_model};

/// Nine tool calls, then the answer: with 10 s tools the requests go out
/// at 0, 10, ..., 90 s into the turn.
const LOOP: &str = r#"
[[rule]]
times = 9
[[rule.tool_calls]]
name = "lookup"
arguments = { q = "more" }

[[rule]]
text = "Done."
"#;

/// The recording model on a route with (or without) a priority tier.
struct Tiered {
    inner: Arc<Recording>,
    tier: bool,
}

impl ModelPort for Tiered {
    fn route(&self) -> BoxFuture<'_, RouteInfo> {
        Box::pin(async move {
            let mut info = self.inner.route().await;
            info.fast_tier = self.tier;
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
        self.inner.call(request, ctx, deltas)
    }
}

async fn speeds(config: &str, tier: bool, slow: bool) -> (Vec<Option<Speed>>, Vec<String>) {
    let profile = Profile::new(config);
    let s = profile.session(Source::Cli);
    let inner = mock_model(&profile, LOOP, "mock").await;
    let model = Arc::new(Tiered {
        inner: inner.clone(),
        tier,
    });
    let tools = Arc::new(FakeTools {
        delay: if slow {
            Duration::from_secs(10)
        } else {
            Duration::ZERO
        },
        ..FakeTools::plain()
    });
    let run = agent(&profile, s, model, tools, |_| {});
    let result = run.turn(&profile, "work").await;
    let speeds = inner.requests().iter().map(|r| r.speed).collect();
    (speeds, result.notices)
}

/// Scenario: Auto window.
#[tokio::test(start_paused = true)]
async fn auto_asks_the_priority_tier_only_within_the_first_minute_of_the_turn() {
    let (speeds, notices) = speeds("[agent]\nfast_mode = \"auto\"\n", true, true).await;
    assert_eq!(speeds.len(), 10);
    // Requests at 0..=50 s are inside the 60 s window, 60..=90 s are not.
    assert!(
        speeds[..6].iter().all(|s| *s == Some(Speed::Fast)),
        "{speeds:?}"
    );
    assert!(speeds[6..].iter().all(Option::is_none), "{speeds:?}");
    assert!(notices.iter().all(|n| n != NO_TIER_NOTICE));
}

#[tokio::test(start_paused = true)]
async fn cold_applies_to_the_first_turn_only_and_fast_to_every_request() {
    let profile = Profile::new("[agent]\nfast_mode = \"cold\"\n");
    let s = profile.session(Source::Cli);
    let inner = mock_model(&profile, "[[rule]]\ntext = \"ok\"\nrepeat = true\n", "mock").await;
    let model = Arc::new(Tiered {
        inner: inner.clone(),
        tier: true,
    });
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    run.turn(&profile, "first").await;
    run.turn(&profile, "second").await;
    let sent: Vec<Option<Speed>> = inner.requests().iter().map(|r| r.speed).collect();
    assert_eq!(sent, vec![Some(Speed::Fast), None]);

    run.agent.set_fast_mode(Some(FastMode::Fast));
    run.turn(&profile, "third").await;
    assert_eq!(inner.requests().last().unwrap().speed, Some(Speed::Fast));
}

#[tokio::test(start_paused = true)]
async fn a_route_without_a_tier_sends_no_tier_and_says_so_once() {
    let (speeds, notices) = speeds("[agent]\nfast_mode = \"fast\"\n", false, false).await;
    assert!(speeds.iter().all(Option::is_none));
    assert_eq!(
        notices.iter().filter(|n| *n == NO_TIER_NOTICE).count(),
        1,
        "{notices:?}"
    );
}
