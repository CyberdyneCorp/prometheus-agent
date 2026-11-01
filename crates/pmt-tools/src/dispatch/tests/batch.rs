use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pmt_core::cancel::CancelToken;
use serde_json::Value;

use super::{Options, harness, tool_with};
use crate::budget::SPILL_KEY;
use crate::descriptor::{
    CallContext, HandlerFuture, PathClaim, RegisteredTool, ResourceClaims, ToolHandler,
};
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

fn with_args(calls: &[(&str, &str)]) -> Vec<ToolCall> {
    calls
        .iter()
        .enumerate()
        .map(|(i, (name, args))| ToolCall::new(&format!("c{i}"), name, args))
        .collect()
}

fn reader(shared: &Shared, name: &str) -> RegisteredTool {
    let mut read = shared.tool(name, 20, 0);
    read.descriptor.claims = Some(ResourceClaims::reads(vec![PathClaim::Argument(
        "/path".into(),
    )]));
    read
}

fn writer(shared: &Shared, name: &str) -> RegisteredTool {
    let mut write = shared.tool(name, 20, 0);
    write.descriptor.claims = Some(ResourceClaims::writes(vec![PathClaim::Argument(
        "/path".into(),
    )]));
    write
}

/// The position of `entry` in the log.
fn at(log: &[String], entry: &str) -> usize {
    log.iter()
        .position(|e| e == entry)
        .unwrap_or_else(|| panic!("{entry} missing from {log:?}"))
}

#[tokio::test(start_paused = true)]
async fn independent_reads_run_together_in_call_order() {
    let shared = Shared::default();
    let harness = harness(vec![reader(&shared, "read_file")], Options::default()).await;
    let calls = with_args(&[
        ("read_file", r#"{"path": "a"}"#),
        ("read_file", r#"{"path": "b"}"#),
        ("read_file", r#"{"path": "c"}"#),
    ]);
    let plan = harness.dispatcher.plan(&calls);
    assert_eq!(plan.edges().count(), 0);
    let results = harness
        .dispatcher
        .run_batch(calls, &CancelToken::new())
        .await;
    assert_eq!(shared.peak.load(Ordering::SeqCst), 3, "all three at once");
    let texts: Vec<String> = results.iter().map(ToolResult::text_content).collect();
    assert_eq!(texts, ["c0:", "c1:", "c2:"]);
}

#[tokio::test(start_paused = true)]
async fn a_writer_waits_for_the_reader_of_its_path_only() {
    let shared = Shared::default();
    let tools = vec![reader(&shared, "read_file"), writer(&shared, "patch")];
    let harness = harness(tools, Options::default()).await;
    let calls = with_args(&[
        ("read_file", r#"{"path": "src/x.rs"}"#),
        ("patch", r#"{"path": "./src/x.rs"}"#),
        ("read_file", r#"{"path": "README.md"}"#),
    ]);
    let plan = harness.dispatcher.plan(&calls);
    assert_eq!(plan.edges().collect::<Vec<_>>(), [(0, 1)]);
    harness
        .dispatcher
        .run_batch(calls, &CancelToken::new())
        .await;
    let log = shared.log();
    assert!(at(&log, "end c0") < at(&log, "start c1"), "{log:?}");
    assert!(at(&log, "start c2") < at(&log, "end c0"), "{log:?}");
}

#[tokio::test(start_paused = true)]
async fn an_interactive_call_is_a_barrier() {
    let shared = Shared::default();
    let mut clarify = shared.tool("clarify", 20, 0);
    clarify.descriptor.claims = Some(ResourceClaims {
        interactive: true,
        ..ResourceClaims::none()
    });
    let harness = harness(
        vec![reader(&shared, "read_file"), clarify],
        Options::default(),
    )
    .await;
    let calls = with_args(&[
        ("read_file", r#"{"path": "a"}"#),
        ("clarify", "{}"),
        ("read_file", r#"{"path": "b"}"#),
    ]);
    harness
        .dispatcher
        .run_batch(calls, &CancelToken::new())
        .await;
    assert_eq!(
        shared.log(),
        [
            "start c0", "end c0", "start c1", "end c1", "start c2", "end c2"
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_tool_without_claims_runs_alone() {
    let shared = Shared::default();
    let mut undeclared = shared.tool("mcp__gh__search", 20, 0);
    undeclared.descriptor.claims = None;
    let harness = harness(
        vec![reader(&shared, "read_file"), undeclared],
        Options::default(),
    )
    .await;
    let calls = with_args(&[
        ("read_file", r#"{"path": "a"}"#),
        ("mcp__gh__search", "{}"),
        ("read_file", r#"{"path": "b"}"#),
    ]);
    let plan = harness.dispatcher.plan(&calls);
    assert_eq!(plan.edges().collect::<Vec<_>>(), [(0, 1), (1, 2)]);
    harness
        .dispatcher
        .run_batch(calls, &CancelToken::new())
        .await;
    assert_eq!(shared.peak.load(Ordering::SeqCst), 1);
    let log = shared.log();
    assert!(at(&log, "end c0") < at(&log, "start c1"), "{log:?}");
    assert!(at(&log, "end c1") < at(&log, "start c2"), "{log:?}");
}

#[tokio::test(start_paused = true)]
async fn two_writes_to_one_path_do_not_overlap() {
    let shared = Shared::default();
    let harness = harness(vec![writer(&shared, "write_file")], Options::default()).await;
    let calls = with_args(&[
        ("write_file", r#"{"path": "out.txt"}"#),
        ("write_file", r#"{"path": "out.txt"}"#),
        ("write_file", "{not json"),
    ]);
    let plan = harness.dispatcher.plan(&calls);
    assert_eq!(plan.deps(1), [0]);
    assert_eq!(plan.deps(2), [0, 1], "unparseable arguments conflict");
    harness
        .dispatcher
        .run_batch(calls, &CancelToken::new())
        .await;
    assert_eq!(shared.peak.load(Ordering::SeqCst), 1);
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
async fn calls_holding_one_exclusive_resource_run_one_at_a_time() {
    let shared = Shared::default();
    let mut browser = shared.tool("browser_exec", 10, 0);
    browser.descriptor.claims = Some(ResourceClaims::exclusive("browser"));
    let harness = harness(
        vec![shared.tool("web_search", 10, 0), browser],
        Options::default(),
    )
    .await;
    let calls = calls(&["browser_exec", "web_search", "browser_exec"]);
    assert_eq!(harness.dispatcher.plan(&calls).deps(2), [0]);
    harness
        .dispatcher
        .run_batch(calls, &CancelToken::new())
        .await;
    let log = shared.log();
    assert!(at(&log, "end c0") < at(&log, "start c2"), "{log:?}");
    assert!(at(&log, "start c1") < at(&log, "end c0"), "{log:?}");
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
