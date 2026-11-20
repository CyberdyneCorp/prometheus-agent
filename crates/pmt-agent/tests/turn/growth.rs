//! A local model's window grows before compression (spec: local-models,
//! Context window and memory placement): at the compression threshold on
//! a local route the growth port is asked first; a grown window skips the
//! compaction, an impossible growth compacts as usual.

use std::sync::{Arc, Mutex};

use pmt_agent::turn::growth::{GROWN_NOTICE, Growth, WindowGrowth};
use pmt_agent::turn::model::{
    BoxFuture, DeltaSink, ModelFailure, ModelPort, ModelReply, RouteInfo,
};
use pmt_obs::trajectory::events::Event;
use pmt_providers::cost::CallContext;
use pmt_providers::request::Request;
use pmt_store::Source;

use crate::compression::{FakeSummarizer, SMALL, committed, exchanges};
use crate::support::{FakeTools, Profile, Recording, agent, mock_model};

/// The recording mock, reported as a local route.
struct Local(Arc<Recording>);

impl ModelPort for Local {
    fn route(&self) -> BoxFuture<'_, RouteInfo> {
        Box::pin(async move {
            let mut info = self.0.route().await;
            info.local = true;
            info
        })
    }

    fn start_turn(&self) -> BoxFuture<'_, ()> {
        self.0.start_turn()
    }

    fn call<'a>(
        &'a self,
        request: &'a Request,
        ctx: &'a CallContext,
        deltas: &'a dyn DeltaSink,
    ) -> BoxFuture<'a, Result<ModelReply, ModelFailure>> {
        self.0.call(request, ctx, deltas)
    }
}

/// Answers every ask with `answer` and keeps the needed token counts.
struct Port {
    answer: Growth,
    asked: Mutex<Vec<u64>>,
}

impl WindowGrowth for Port {
    fn grow<'a>(
        &'a self,
        _session: &'a str,
        _route: &'a RouteInfo,
        needed: u64,
    ) -> BoxFuture<'a, Growth> {
        self.asked.lock().unwrap().push(needed);
        Box::pin(async move { self.answer.clone() })
    }
}

async fn turn_at_the_threshold(answer: Growth) -> (Vec<u64>, usize, Vec<String>) {
    let profile = Profile::new(SMALL);
    let s = profile.session(Source::Cli);
    // About 15,000 tokens of history: over the 13,600-token trigger.
    exchanges(&profile, s, 20, 3_000);
    // A tool round first: the next request is measured from the
    // provider's reported usage, then evaluated against the trigger.
    let script = "[[rule]]\ntimes = 1\n[[rule.tool_calls]]\nname = \"lookup\"\narguments = { q = \"x\" }\n\n[[rule]]\ntext = \"ok\"\nrepeat = true\n";
    let inner = mock_model(&profile, script, "mock").await;
    let port = Arc::new(Port {
        answer,
        asked: Mutex::default(),
    });
    let summarizer = FakeSummarizer::new();
    let growth: Arc<dyn WindowGrowth> = port.clone();
    let run = agent(
        &profile,
        s,
        Arc::new(Local(inner)),
        FakeTools::new(),
        move |setup| {
            setup.summarizer = Some(summarizer);
            setup.growth = Some(growth);
        },
    );
    let result = run.turn(&profile, "next question").await;
    let compressions = committed(&run).len()
        + run
            .events
            .events()
            .iter()
            .filter(|e| matches!(e, Event::CompressionStarted(_)))
            .count();
    let asked = port.asked.lock().unwrap().clone();
    (asked, compressions, result.notices)
}

/// Scenario: Window grows before compression.
#[tokio::test]
async fn a_grown_window_skips_the_compaction() {
    let (asked, compressions, notices) = turn_at_the_threshold(Growth::Grown {
        from: 16_000,
        to: 32_000,
    })
    .await;
    assert_eq!(asked.len(), 1);
    assert!(asked[0] >= 13_600, "{asked:?}");
    assert_eq!(compressions, 0);
    assert!(
        notices
            .iter()
            .any(|n| n.starts_with(GROWN_NOTICE) && n.contains("16000 -> 32000")),
        "{notices:?}"
    );
}

/// Scenario: Growth impossible triggers compression.
#[tokio::test]
async fn an_impossible_growth_compacts_as_usual() {
    let (asked, compressions, _) =
        turn_at_the_threshold(Growth::Impossible("native_max".to_owned())).await;
    assert_eq!(asked.len(), 1);
    assert!(compressions > 0);
}
