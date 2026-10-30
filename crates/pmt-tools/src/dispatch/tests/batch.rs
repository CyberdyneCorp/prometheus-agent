use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pmt_core::cancel::CancelToken;
use serde_json::Value;

use super::{Options, harness, tool_with};
use crate::budget::SPILL_KEY;
use crate::descriptor::{CallContext, HandlerFuture, PathClaim, RegisteredTool, ToolHandler};
use crate::dispatch::ToolCall;
use crate::result::{Status, ToolResult};
use crate::spillover::session_dir;

/// Logs `start <call>` / `end <call>` around a sleep and tracks how many
/// calls are in flight.
struct Sleeper {
    log: Arc<Mutex<Vec<String>>>,
    sleep: Duration,
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    output: usize,
}

impl ToolHandler for Sleeper {
    fn call<'a>(&'a self, context: &'a CallContext, _: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let id = &context.call_id;
            self.log.lock().unwrap().push(format!("start {id}"));
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(self.sleep).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            self.log.lock().unwrap().push(format!("end {id}"));
            Ok(ToolResult::ok(format!("{id}:{}", "o".repeat(self.output))))
        })
    }
}

#[derive(Default)]
struct Shared {
    log: Arc<Mutex<Vec<String>>>,
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl Shared {
    fn tool(&self, name: &str, sleep_ms: u64, output: usize) -> RegisteredTool {
        tool_with(
            name,
            Arc::new(Sleeper {
                log: Arc::clone(&self.log),
                sleep: Duration::from_millis(sleep_ms),
                in_flight: Arc::clone(&self.in_flight),
                peak: Arc::clone(&self.peak),
                output,
            }),
        )
    }
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

fn calls(names: &[&str]) -> Vec<ToolCall> {
    names
        .iter()
        .enumerate()
        .map(|(i, name)| ToolCall::new(&format!("c{i}"), name, "{}"))
        .collect()
}

fn writer(shared: &Shared) -> RegisteredTool {
    let mut write = shared.tool("write_file", 20, 0);
    write.descriptor.claims.writes = vec![PathClaim::Argument("/path".into())];
    write
}

#[tokio::test(start_paused = true)]
async fn mixed_batch_runs_sequentially_in_order() {
    let shared = Shared::default();
    let tools = vec![shared.tool("read_file", 20, 0), writer(&shared)];
    let harness = harness(tools, Options::default()).await;
    let results = harness
        .dispatcher
        .run_batch(
            calls(&["read_file", "read_file", "write_file"]),
            &CancelToken::new(),
        )
        .await;
    assert_eq!(
        shared.log(),
        [
            "start c0", "end c0", "start c1", "end c1", "start c2", "end c2"
        ]
    );
    let texts: Vec<String> = results.iter().map(ToolResult::text_content).collect();
    assert_eq!(texts, ["c0:", "c1:", "c2:"]);
}

#[tokio::test(start_paused = true)]
async fn read_only_batch_runs_concurrently_within_the_bound_in_call_order() {
    let shared = Shared::default();
    let tools = vec![
        shared.tool("read_file", 50, 0),
        shared.tool("web_search", 10, 0),
    ];
    let options = Options {
        concurrency: 2,
        ..Options::default()
    };
    let harness = harness(tools, options).await;
    let names = ["read_file", "read_file", "web_search", "read_file"];
    let results = harness
        .dispatcher
        .run_batch(calls(&names), &CancelToken::new())
        .await;
    assert_eq!(
        shared.peak.load(Ordering::SeqCst),
        2,
        "bounded by agent.tool_concurrency"
    );
    let log = shared.log();
    assert!(log[..2].iter().all(|e| e.starts_with("start")), "{log:?}");
    let texts: Vec<String> = results.iter().map(ToolResult::text_content).collect();
    assert_eq!(texts, ["c0:", "c1:", "c2:", "c3:"], "original call order");
}

#[tokio::test(start_paused = true)]
async fn exclusive_or_interactive_claims_force_sequential_order() {
    for claim in ["exclusive", "interactive"] {
        let shared = Shared::default();
        let mut special = shared.tool("clarify", 10, 0);
        if claim == "exclusive" {
            special.descriptor.claims.exclusive = vec!["browser".into()];
        } else {
            special.descriptor.claims.interactive = true;
        }
        let harness = harness(
            vec![shared.tool("read_file", 10, 0), special],
            Options::default(),
        )
        .await;
        harness
            .dispatcher
            .run_batch(calls(&["read_file", "clarify"]), &CancelToken::new())
            .await;
        assert_eq!(shared.peak.load(Ordering::SeqCst), 1, "{claim}");
    }
}

#[tokio::test(start_paused = true)]
async fn calls_are_bounded_by_their_timeout() {
    let shared = Shared::default();
    let mut slow = shared.tool("web_extract", 10_000, 0);
    slow.descriptor.timeout = Some(Duration::from_secs(2));
    let mut fallback = shared.tool("web_search", 400_000, 0);
    fallback.descriptor.timeout = None; // tools.default_timeout_secs = 300
    let harness = harness(vec![slow, fallback], Options::default()).await;
    let results = harness
        .dispatcher
        .run_batch(calls(&["web_extract", "web_search"]), &CancelToken::new())
        .await;
    assert_eq!(results[0].status, Status::Timeout);
    assert_eq!(results[0].text_content(), "web_extract timed out after 2s");
    assert_eq!(results[1].text_content(), "web_search timed out after 300s");
}

#[tokio::test]
async fn interrupt_cancels_in_flight_calls_within_two_seconds() {
    let shared = Shared::default();
    let harness = harness(
        vec![shared.tool("web_extract", 60_000, 0)],
        Options::default(),
    )
    .await;
    let cancel = CancelToken::new();
    let interrupt = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        interrupt.cancel();
    });
    let started = Instant::now();
    let results = harness
        .dispatcher
        .run_batch(calls(&["web_extract", "web_extract"]), &cancel)
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    for result in &results {
        assert_eq!(result.status, Status::Cancelled);
        assert_eq!(result.text_content(), "web_extract was cancelled");
    }
}

#[tokio::test]
async fn oversized_result_is_replaced_by_a_spill_reference() {
    let shared = Shared::default();
    let harness = harness(
        vec![shared.tool("web_extract", 0, 150_000)],
        Options::default(),
    )
    .await;
    let result = harness
        .dispatcher
        .dispatch(
            ToolCall::new("call_big", "web_extract", "{}"),
            &CancelToken::new(),
        )
        .await;
    let text = result.text_content();
    assert!(text.starts_with("<persisted-output>"), "{}", &text[..80]);
    let path = session_dir(harness.profile.path(), "ses_test").join("call_big.txt");
    assert_eq!(result.metadata[SPILL_KEY], path.display().to_string());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap().chars().count(),
        150_000 + "call_big:".len()
    );
    assert!(
        harness
            .recorder
            .events()
            .contains(&"tool.result call_big web_extract ok".to_owned())
    );
}

#[tokio::test]
async fn one_responses_results_share_the_turn_budget() {
    let shared = Shared::default();
    let harness = harness(
        vec![
            shared.tool("web_search", 0, 80_000),
            shared.tool("web_extract", 0, 90_000),
        ],
        Options::default(),
    )
    .await;
    let results = harness
        .dispatcher
        .run_batch(
            calls(&["web_search", "web_extract", "web_search"]),
            &CancelToken::new(),
        )
        .await;
    // 250k chars over the 200k turn budget: the largest (web_extract) spills.
    let spilled: Vec<bool> = results
        .iter()
        .map(|r| r.text_content().starts_with("<persisted-output>"))
        .collect();
    assert_eq!(spilled, [false, true, false]);
}
