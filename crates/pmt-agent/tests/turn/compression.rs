//! Context compression inside the turn loop and through `/compress`
//! (spec: context-compression): trigger points and the attempt budget,
//! provider usage over estimates, overflow recovery, the summary and its
//! failures, in-place commits, the manual command and the cache
//! breakpoint plan.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use jiff::Timestamp;
use pmt_agent::compress::manual::{self, Manual};
use pmt_agent::compress::summary::{FOOTER, HEADER};
use pmt_agent::compress::{FailureKind, Status, Summarizer, SummaryFailure};
use pmt_agent::turn::model::{
    BoxFuture, DeltaSink, ModelFailure, ModelPort, ModelReply, RouteInfo,
};
use pmt_agent::turn::{CONTEXT_FULL, ExitReason};
use pmt_config::schema::WireFormat;
use pmt_core::id::SessionId;
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::{CompressionCommitted, CompressionFailed};
use pmt_providers::cost::CallContext;
use pmt_providers::request::Request;
use pmt_store::{
    Acquire, CompressionState, Holder, LeaseKind, Message, NewMessage, Role, SearchOptions, Source,
    Visibility,
};
use pmt_tools::ToolResult;

use crate::support::{FakeTools, Profile, Recording, Run, agent, mock_model, system_text};

/// A 16K-token mock model: a 3,200-token tail budget, a 13,600-token
/// trigger.
const SMALL: &str = "[models.\"mock/mock\"]\ncontext_window = 16000\n";

/// Answers every summary request with the next scripted result (then a
/// fixed summary) and keeps the requests.
#[derive(Default)]
struct FakeSummarizer {
    script: Mutex<VecDeque<Result<String, SummaryFailure>>>,
    requests: Mutex<Vec<Request>>,
    delay: Duration,
}

impl FakeSummarizer {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn failing(kind: FailureKind) -> Arc<Self> {
        let fake = Self::default();
        fake.script
            .lock()
            .unwrap()
            .push_back(Err(SummaryFailure::new(kind, "scripted failure")));
        Arc::new(fake)
    }

    fn texts(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|r| match &r.messages[0].parts[0] {
                pmt_providers::request::Part::Text { text } => text.clone(),
                _ => String::new(),
            })
            .collect()
    }
}

impl Summarizer for FakeSummarizer {
    fn summarize<'a>(
        &'a self,
        request: &'a Request,
    ) -> BoxFuture<'a, Result<String, SummaryFailure>> {
        Box::pin(async move {
            let n = {
                let mut requests = self.requests.lock().unwrap_or_else(PoisonError::into_inner);
                requests.push(request.clone());
                requests.len()
            };
            tokio::time::sleep(self.delay).await;
            let next = self
                .script
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front();
            next.unwrap_or_else(|| Ok(format!("## Goal\nGoal S{n}\n## Progress\nDid S{n}.")))
        })
    }
}

fn with_summarizer(
    summarizer: &Arc<FakeSummarizer>,
) -> impl FnOnce(&mut pmt_agent::turn::AgentSetup) {
    let summarizer = summarizer.clone();
    move |setup| setup.summarizer = Some(summarizer)
}

fn committed(run: &Run) -> Vec<CompressionCommitted> {
    run.events
        .events()
        .into_iter()
        .filter_map(|e| match e {
            Event::CompressionCommitted(c) => Some(c),
            _ => None,
        })
        .collect()
}

fn failures(run: &Run) -> Vec<CompressionFailed> {
    run.events
        .events()
        .into_iter()
        .filter_map(|e| match e {
            Event::CompressionFailed(f) => Some(f),
            _ => None,
        })
        .collect()
}

fn rebuilds(run: &Run) -> Vec<String> {
    run.events
        .events()
        .into_iter()
        .filter_map(|e| match e {
            Event::PromptRebuilt(p) => Some(p.reason),
            _ => None,
        })
        .collect()
}

fn active(profile: &Profile, s: SessionId) -> Vec<Message> {
    profile.store.messages(s, Visibility::Active).unwrap()
}

fn texts(rows: &[Message]) -> Vec<String> {
    rows.iter()
        .map(|m| m.content.clone().unwrap_or_default())
        .collect()
}

fn no_adjacent_roles(rows: &[Message]) -> bool {
    let side = |r: Role| matches!(r, Role::Assistant);
    rows.windows(2)
        .all(|w| side(w[0].role) != side(w[1].role) || w[1].role == Role::Tool)
}

/// `n` stored user/assistant exchanges of `size` characters each.
fn exchanges(profile: &Profile, s: SessionId, n: usize, size: usize) {
    let rows: Vec<NewMessage> = (0..n)
        .flat_map(|i| {
            [
                NewMessage::new(Role::User, format!("question {i} {}", "q".repeat(size))),
                NewMessage::new(Role::Assistant, format!("answer {i}")),
            ]
        })
        .collect();
    profile.store.append_messages(s, &rows).unwrap();
}

fn cooldown(profile: &Profile, s: SessionId, secs: i64) {
    let state = CompressionState {
        cooldown_until: Some(
            Timestamp::now()
                .checked_add(jiff::SignedDuration::from_secs(secs))
                .unwrap(),
        ),
        ..CompressionState::default()
    };
    profile.store.save_compression_state(s, &state).unwrap();
}

const TOOL_TURN: &str = r#"
[[rule]]
user = "four"
times = 1
[[rule.tool_calls]]
name = "lookup"
arguments = { q = "x" }

[[rule]]
text = "ok"
repeat = true
"#;

#[tokio::test]
async fn tool_output_over_the_threshold_compacts_post_tool_in_place() {
    let mut profile = Profile::new(&format!("{SMALL}[compression]\nenabled = false\n"));
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, TOOL_TURN, "mock").await;
    let tools = FakeTools::answering(|_| ToolResult::ok("y".repeat(16_000)));
    let summarizer = FakeSummarizer::new();
    let run = agent(
        &profile,
        s,
        model.clone(),
        tools,
        with_summarizer(&summarizer),
    );
    let long = "w".repeat(2_000);
    run.turn(&profile, &format!("one {long}")).await;
    run.turn(&profile, &format!("two {long}")).await;
    run.turn(&profile, &format!("three pelican {long}")).await;
    let small = model.requests().last().unwrap().estimated_tokens();
    profile.reconfigure(&format!(
        "{SMALL}[compression]\nthreshold_tokens = {}\n",
        small + 1_000
    ));

    let result = run.turn(&profile, "four").await;

    assert_eq!(result.exit_reason, ExitReason::Completed);
    let done = committed(&run);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0].trigger, "post_tool");
    assert_eq!(done[0].epoch, 1);
    assert!(done[0].tokens_after < done[0].tokens_before);
    // The summarizer read the middle and the summary is in place.
    assert!(summarizer.texts()[0].contains("[USER] three pelican "));
    let rows = active(&profile, s);
    let summary = rows
        .iter()
        .find(|m| m.content.as_deref().unwrap_or_default().starts_with(HEADER))
        .unwrap();
    assert!(summary.content.as_deref().unwrap().ends_with(FOOTER));
    assert_eq!(
        summary.metadata.as_ref().unwrap()["compaction_summary"],
        true
    );
    assert!(no_adjacent_roles(&rows));
    // Same session; the replaced rows are archived and searchable.
    assert_eq!(profile.store.session(s).unwrap().compaction_count, 1);
    let archived: Vec<Message> = profile
        .store
        .messages(s, Visibility::All)
        .unwrap()
        .into_iter()
        .filter(|m| m.compacted)
        .collect();
    assert!(archived.iter().all(|m| m.compaction_epoch == Some(1)));
    let hits = profile
        .store
        .search("pelican", &SearchOptions::default())
        .unwrap();
    assert!(hits.hits.iter().any(|h| h.session_id == s));
    // The prompt was rebuilt at the commit, before the next request.
    assert_eq!(rebuilds(&run), ["new_session", "compaction"]);
}

#[tokio::test]
async fn the_attempt_budget_caps_compactions_per_turn() {
    let script = r#"
[[rule]]
user = "four"
times = 3
[[rule.tool_calls]]
name = "lookup"
arguments = { q = "x" }

[[rule]]
text = "ok"
repeat = true
"#;
    let mut profile = Profile::new(&format!("{SMALL}[compression]\nenabled = false\n"));
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, script, "mock").await;
    let tools = FakeTools::answering(|_| ToolResult::ok("y".repeat(16_000)));
    let summarizer = FakeSummarizer::new();
    let run = agent(
        &profile,
        s,
        model.clone(),
        tools,
        with_summarizer(&summarizer),
    );
    for text in ["one", "two", "three"] {
        run.turn(&profile, text).await;
    }
    let small = model.requests().last().unwrap().estimated_tokens();
    profile.reconfigure(&format!(
        "{SMALL}[compression]\nthreshold_tokens = {}\nmax_attempts = 1\n",
        small + 1_000
    ));
    let result = run.turn(&profile, "four").await;
    assert_eq!(result.exit_reason, ExitReason::Completed);
    // Still over the threshold after every later tool round, but the one
    // attempt of the turn is spent.
    assert_eq!(committed(&run).len(), 1);
}

#[tokio::test]
async fn a_resumed_session_sends_its_first_request_uncompressed() {
    let profile = Profile::new(&format!("{SMALL}[compression]\nthreshold_tokens = 2000\n"));
    let s = profile.session(Source::Cli);
    exchanges(&profile, s, 6, 4_000);
    let model = mock_model(&profile, "[[rule]]\ntext = \"ok\"\nrepeat = true\n", "mock").await;
    let summarizer = FakeSummarizer::new();
    let run = agent(
        &profile,
        s,
        model.clone(),
        FakeTools::new(),
        with_summarizer(&summarizer),
    );

    run.turn(&profile, "hello again").await;
    // Only an estimate was over: the request went out whole.
    assert!(committed(&run).is_empty());
    assert_eq!(model.requests()[0].messages.len(), 13);

    // The reported usage is over too: the next turn compacts first.
    run.turn(&profile, "and now").await;
    let done = committed(&run);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].trigger, "turn_start");
    let requests = model.requests();
    assert!(requests[1].estimated_tokens() < requests[0].estimated_tokens());
}

const OVERFLOW: &str = r#"
[[rule]]
user = "four"
fault = "context_overflow"
times = 1

[[rule]]
text = "ok"
repeat = true
"#;

#[tokio::test]
async fn an_overflow_compacts_and_retries_despite_an_armed_cooldown() {
    let profile = Profile::new(&format!("{SMALL}[compression]\nprotect_first_n = 0\n"));
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, OVERFLOW, "mock").await;
    let summarizer = FakeSummarizer::new();
    let run = agent(
        &profile,
        s,
        model.clone(),
        FakeTools::new(),
        with_summarizer(&summarizer),
    );
    for text in ["one", "two", "three"] {
        run.turn(&profile, &format!("{text} {}", "z".repeat(6_000)))
            .await;
    }
    cooldown(&profile, s, 600);

    let result = run.turn(&profile, "four").await;

    assert_eq!(result.exit_reason, ExitReason::Completed);
    assert_eq!(result.final_response.as_deref(), Some("ok"));
    let requests = model.requests();
    assert_eq!(requests.len(), 5, "three turns, the overflow, the retry");
    assert!(requests[4].estimated_tokens() < requests[3].estimated_tokens());
    let done = committed(&run);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].trigger, "overflow");
    // The cooldown held for automatic triggers is still armed.
    let state = profile.store.compression_state(s).unwrap();
    assert!(state.cooldown_until.unwrap() > Timestamp::now());
    // The retry took no extra iteration.
    assert_eq!(result.iterations, 1);
}

#[tokio::test]
async fn an_overflow_after_the_attempts_ends_with_context_exhausted() {
    let script = "[[rule]]\nuser = \"four\"\nfault = \"context_overflow\"\nrepeat = true\n\n[[rule]]\ntext = \"ok\"\nrepeat = true\n";
    let profile = Profile::new(&format!(
        "{SMALL}[compression]\nprotect_first_n = 0\nmax_attempts = 1\n"
    ));
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, script, "mock").await;
    let summarizer = FakeSummarizer::new();
    let run = agent(
        &profile,
        s,
        model.clone(),
        FakeTools::new(),
        with_summarizer(&summarizer),
    );
    for text in ["one", "two", "three"] {
        run.turn(&profile, &format!("{text} {}", "z".repeat(6_000)))
            .await;
    }
    let result = run.turn(&profile, "four").await;
    assert_eq!(result.exit_reason, ExitReason::ContextExhausted);
    assert_eq!(result.error.unwrap().message(), CONTEXT_FULL);
    assert_eq!(model.requests().len(), 5, "the overflow and one retry");
    assert_eq!(committed(&run).len(), 1);
}

#[tokio::test]
async fn a_second_compaction_updates_the_previous_summary() {
    let profile = Profile::new(SMALL);
    let s = profile.session(Source::Cli);
    exchanges(&profile, s, 6, 10);
    let model = mock_model(&profile, "[[rule]]\ntext = \"ok\"\nrepeat = true\n", "mock").await;
    let summarizer = FakeSummarizer::new();
    let run = agent(
        &profile,
        s,
        model,
        FakeTools::new(),
        with_summarizer(&summarizer),
    );
    let here = manual::parse("here 1").unwrap();
    let first = run.agent.compress(&profile.scope, &here).await.unwrap();
    assert_eq!(first.status, Status::Compressed);
    exchanges(&profile, s, 2, 10);
    let second = run.agent.compress(&profile.scope, &here).await.unwrap();
    assert_eq!(second.status, Status::Compressed);
    assert_eq!(second.epoch, Some(2));
    let texts = summarizer.texts();
    assert!(!texts[0].contains("PREVIOUS SUMMARY"));
    assert!(texts[1].contains("PREVIOUS SUMMARY"));
    assert!(texts[1].contains("Goal S1"));
    // The old summary was summarized too: one summary row remains.
    let rows = active(&profile, s);
    let summaries = rows
        .iter()
        .filter(|m| m.content.as_deref().unwrap_or_default().starts_with(HEADER))
        .count();
    assert_eq!(summaries, 1);
    assert!(no_adjacent_roles(&rows));
}

#[tokio::test]
async fn compress_here_keeps_the_last_two_exchanges_verbatim() {
    let profile = Profile::new(SMALL);
    let s = profile.session(Source::Cli);
    exchanges(&profile, s, 30, 10);
    let before = active(&profile, s);
    let model = mock_model(&profile, "[[rule]]\ntext = \"ok\"\n", "mock").await;
    let summarizer = FakeSummarizer::new();
    let run = agent(
        &profile,
        s,
        model,
        FakeTools::new(),
        with_summarizer(&summarizer),
    );

    let outcome = run
        .agent
        .compress(&profile.scope, &manual::parse("here").unwrap())
        .await
        .unwrap();

    assert_eq!(outcome.status, Status::Compressed);
    assert_eq!(outcome.summarized.len(), 56);
    let rows = active(&profile, s);
    assert_eq!(rows.len(), 5);
    let kept: Vec<_> = rows[1..].iter().map(|m| m.id).collect();
    let last: Vec<_> = before[56..].iter().map(|m| m.id).collect();
    assert_eq!(kept, last);
    assert_eq!(rows[0].role, Role::Assistant);
    assert!(manual::render(&outcome).starts_with("compressed: 56 messages summarized"));
}

#[tokio::test]
async fn preview_and_lock_skipped_leave_the_transcript_unchanged() {
    let profile = Profile::new(SMALL);
    let s = profile.session(Source::Cli);
    exchanges(&profile, s, 10, 10);
    let before = texts(&active(&profile, s));
    let model = mock_model(&profile, "[[rule]]\ntext = \"ok\"\n", "mock").await;
    let summarizer = FakeSummarizer::new();
    let run = agent(
        &profile,
        s,
        model,
        FakeTools::new(),
        with_summarizer(&summarizer),
    );

    let preview = Manual {
        preview: true,
        keep_exchanges: Some(2),
        focus: None,
    };
    let outcome = run.agent.compress(&profile.scope, &preview).await.unwrap();
    assert_eq!(outcome.status, Status::Preview);
    assert_eq!(outcome.summarized.len(), 16);
    assert_eq!(texts(&active(&profile, s)), before);
    assert!(summarizer.texts().is_empty());

    // Gateway hygiene (another holder) has the lock.
    let Acquire::Granted(lock) = profile
        .store
        .acquire_lease(s, LeaseKind::Compaction, &Holder::current())
        .unwrap()
    else {
        panic!("busy");
    };
    let outcome = run
        .agent
        .compress(&profile.scope, &manual::parse("here").unwrap())
        .await
        .unwrap();
    assert_eq!(outcome.status, Status::LockSkipped);
    assert_eq!(texts(&active(&profile, s)), before);
    profile.store.release_lease(&lock).unwrap();
}

#[tokio::test]
async fn manual_compress_clears_the_cooldown_and_asks_for_a_summary() {
    let profile = Profile::new(SMALL);
    let s = profile.session(Source::Cli);
    exchanges(&profile, s, 5, 10);
    cooldown(&profile, s, 300);
    let model = mock_model(&profile, "[[rule]]\ntext = \"ok\"\n", "mock").await;
    let summarizer = FakeSummarizer::new();
    let run = agent(
        &profile,
        s,
        model,
        FakeTools::new(),
        with_summarizer(&summarizer),
    );
    let focus = manual::parse("here 1 the parser").unwrap();
    let outcome = run.agent.compress(&profile.scope, &focus).await.unwrap();
    assert_eq!(outcome.status, Status::Compressed);
    assert_eq!(summarizer.texts().len(), 1);
    assert!(
        summarizer.texts()[0]
            .contains("PRIORITY FROM THE USER - preserve everything about this first: the parser")
    );
    assert_eq!(
        profile.store.compression_state(s).unwrap().cooldown_until,
        None
    );
}

#[tokio::test]
async fn a_failed_summary_arms_the_cooldown_and_commits_the_fallback() {
    let profile = Profile::new(SMALL);
    let s = profile.session(Source::Cli);
    exchanges(&profile, s, 5, 10);
    let model = mock_model(&profile, "[[rule]]\ntext = \"ok\"\n", "mock").await;
    let summarizer = FakeSummarizer::failing(FailureKind::Timeout);
    let run = agent(
        &profile,
        s,
        model,
        FakeTools::new(),
        with_summarizer(&summarizer),
    );
    let outcome = run
        .agent
        .compress(&profile.scope, &manual::parse("here 1").unwrap())
        .await
        .unwrap();
    assert_eq!(outcome.status, Status::Compressed);
    assert!(outcome.fallback);
    assert_eq!(failures(&run)[0].reason, "summary_timeout");
    let state = profile.store.compression_state(s).unwrap();
    assert_eq!(state.summary_failures, 1);
    let left = state
        .cooldown_until
        .unwrap()
        .duration_since(Timestamp::now());
    assert!(left.as_secs() > 50 && left.as_secs() <= 60, "{left:?}");
    let summary = active(&profile, s)[0].content.clone().unwrap();
    assert!(summary.contains("deterministic record"), "{summary}");
    assert!(summary.contains("Latest user request: question 3"));
}

#[tokio::test]
async fn a_summary_over_its_time_limit_is_a_timeout() {
    let profile = Profile::new(&format!("{SMALL}[compression]\nsummary_timeout_secs = 1\n"));
    let s = profile.session(Source::Cli);
    exchanges(&profile, s, 5, 10);
    let model = mock_model(&profile, "[[rule]]\ntext = \"ok\"\n", "mock").await;
    let summarizer = Arc::new(FakeSummarizer {
        delay: Duration::from_secs(5),
        ..FakeSummarizer::default()
    });
    let run = agent(
        &profile,
        s,
        model,
        FakeTools::new(),
        with_summarizer(&summarizer),
    );
    let outcome = run
        .agent
        .compress(&profile.scope, &manual::parse("here 1").unwrap())
        .await
        .unwrap();
    assert_eq!(outcome.failure.unwrap().kind, FailureKind::Timeout);
}

#[tokio::test]
async fn an_auth_failure_aborts_with_the_transcript_unchanged() {
    let profile = Profile::new(SMALL);
    let s = profile.session(Source::Cli);
    exchanges(&profile, s, 5, 10);
    let before = texts(&active(&profile, s));
    let model = mock_model(&profile, "[[rule]]\ntext = \"ok\"\n", "mock").await;
    let summarizer = FakeSummarizer::failing(FailureKind::Auth);
    let run = agent(
        &profile,
        s,
        model,
        FakeTools::new(),
        with_summarizer(&summarizer),
    );
    let outcome = run
        .agent
        .compress(&profile.scope, &manual::parse("here 1").unwrap())
        .await
        .unwrap();
    assert_eq!(outcome.status, Status::Failed);
    assert_eq!(texts(&active(&profile, s)), before);
    assert_eq!(profile.store.session(s).unwrap().compaction_count, 0);
    assert!(
        profile
            .store
            .compression_state(s)
            .unwrap()
            .cooldown_until
            .is_some()
    );
}

#[tokio::test]
async fn a_commit_rebuilds_the_prompt_from_the_current_memory() {
    let profile = Profile::new(SMALL);
    let s = profile.session(Source::Cli);
    let memory = profile.dir().join("memory-test.md");
    std::fs::write(&memory, "remember MEM-ALPHA").unwrap();
    let model = mock_model(&profile, "[[rule]]\ntext = \"ok\"\nrepeat = true\n", "mock").await;
    let summarizer = FakeSummarizer::new();
    let path = memory.clone();
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |setup| {
        setup.summarizer = Some(summarizer.clone());
        setup.refresh_prompt = Some(Arc::new(move |p| {
            p.memory = std::fs::read_to_string(&path).ok();
        }));
    });
    run.turn(&profile, "one").await;
    std::fs::write(&memory, "remember MEM-BETA").unwrap();
    run.turn(&profile, "two").await;
    assert!(system_text(&model.requests()[1]).contains("MEM-ALPHA"));
    run.agent
        .compress(&profile.scope, &manual::parse("here 1").unwrap())
        .await
        .unwrap();
    run.turn(&profile, "three").await;
    let last = model.requests().last().unwrap().clone();
    assert!(system_text(&last).contains("MEM-BETA"));
    assert_eq!(rebuilds(&run), ["new_session", "compaction"]);
}

/// The mock model presented as an Anthropic Messages route.
struct AsAnthropic(Arc<Recording>);

impl ModelPort for AsAnthropic {
    fn route(&self) -> BoxFuture<'_, RouteInfo> {
        Box::pin(async move {
            let mut info = self.0.route().await;
            info.wire_format = WireFormat::AnthropicMessages;
            info
        })
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

#[tokio::test]
async fn requests_carry_the_planned_breakpoints_and_only_compaction_breaks_the_cache() {
    use pmt_agent::turn::breakpoints::count;
    let profile = Profile::new(&format!(
        "{SMALL}[prompt_cache]\nttl = \"auto\"\n\n[providers.ant]\nbase_url = \"http://127.0.0.1:9/ant\"\nwire_format = \"anthropic_messages\"\nkey_secret = \"ANT_KEY\"\n"
    ));
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, "[[rule]]\ntext = \"ok\"\nrepeat = true\n", "mock").await;
    let summarizer = FakeSummarizer::new();
    let port: Arc<dyn ModelPort> = Arc::new(AsAnthropic(model.clone()));
    let run = agent(
        &profile,
        s,
        port,
        FakeTools::new(),
        with_summarizer(&summarizer),
    );
    for text in ["one", "two", "three"] {
        run.turn(&profile, text).await;
    }
    run.agent
        .compress(&profile.scope, &manual::parse("here 1").unwrap())
        .await
        .unwrap();
    for text in ["four", "five"] {
        run.turn(&profile, text).await;
    }

    let requests = model.requests();
    assert_eq!(count(&requests[0]), 3, "two tiers and the only row");
    for request in &requests[1..] {
        assert_eq!(count(request), 4);
        assert_eq!(
            request.system.iter().filter(|b| b.cache_breakpoint).count(),
            2
        );
        let n = request.messages.len();
        assert!(request.messages[n - 2..].iter().all(|m| m.cache_breakpoint));
    }
    // On the wire: four one-hour markers (`auto` on the CLI).
    let route = anthropic_route(&profile).await;
    let body = pmt_providers::transport::body(&route, requests.last().unwrap());
    let wire = body.to_string();
    assert_eq!(wire.matches("cache_control").count(), 4);
    assert_eq!(wire.matches(r#""ttl":"1h""#).count(), 4);
    // The compaction is the only break, and a recorded one.
    let breaks = run
        .events
        .events()
        .into_iter()
        .filter(|e| matches!(e, Event::CacheUnexpectedBreak(_)))
        .count();
    assert_eq!(breaks, 0);
    assert_eq!(rebuilds(&run), ["new_session", "compaction"]);
    // Nothing on disk changed: the rebuilt bytes equal the stored ones,
    // which are kept.
    let stored = profile.store.session_prompt(s).unwrap().unwrap();
    assert_eq!(stored.reason, "new_session");
}

async fn anthropic_route(profile: &Profile) -> pmt_providers::Route {
    use std::collections::BTreeMap;

    use pmt_core::secret::SecretString;
    use pmt_providers::resolve::{NoExtras, Resolver, Target, TcpProbe, Trace};
    let config = profile.scope.config();
    let registry = pmt_providers::ProviderRegistry::for_profile(config, Vec::new()).unwrap();
    let secrets = BTreeMap::from([(
        "ANT_KEY".to_owned(),
        SecretString::new("ant-key-0123456789"),
    )]);
    let resolver = Resolver {
        registry: &registry,
        config,
        secrets: &secrets,
        extras: &NoExtras,
        probe: &TcpProbe {
            timeout: Duration::from_millis(100),
        },
        ad_hoc: None,
    };
    let target = Target {
        provider: "ant".into(),
        model: Some("m".into()),
        ..Target::default()
    };
    resolver
        .resolve_target(&target, &mut Trace::default())
        .await
        .unwrap()
}

#[tokio::test]
async fn the_summary_comes_from_the_auxiliary_compression_route() {
    use std::collections::BTreeMap;

    use pmt_agent::compress::summary::AuxSummarizer;
    use pmt_core::secret::SecretString;
    use pmt_obs::usage::{Prices, UsageRecorder};
    use pmt_providers::auxiliary::{AuxCaller, AuxRoute, AuxSource, AuxTask};
    use pmt_providers::metadata::Catalog;
    use pmt_providers::mock::{Mock, Script};
    use pmt_providers::resolve::{NoExtras, Resolver, Target, TcpProbe, Trace};

    let profile = Profile::new(SMALL);
    let s = profile.session(Source::Cli);
    exchanges(&profile, s, 4, 10);
    let config = profile.scope.config();
    let registry = pmt_providers::ProviderRegistry::for_profile(config, Vec::new()).unwrap();
    let secrets: BTreeMap<String, SecretString> = BTreeMap::new();
    let resolver = Resolver {
        registry: &registry,
        config,
        secrets: &secrets,
        extras: &NoExtras,
        probe: &TcpProbe {
            timeout: Duration::from_millis(100),
        },
        ad_hoc: None,
    };
    let target = Target {
        provider: "mock".into(),
        model: Some("summarizer".into()),
        ..Target::default()
    };
    let route = resolver
        .resolve_target(&target, &mut Trace::default())
        .await
        .unwrap();
    let aux = AuxRoute {
        task: AuxTask::Compression,
        route,
        source: AuxSource::Config,
        timeout: Duration::from_secs(5),
        retries: 0,
    };
    let script = Script::parse(
        "[[rule]]\ntext = \"## Goal\\nAUX-GOAL\\n## Next Steps\\nAUX-NEXT\"\n",
        "summary.toml",
    )
    .unwrap();
    let prices: Arc<dyn Prices> = Arc::new(Catalog::new(config, None));
    let caller = AuxCaller::new(pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap())
        .with_mock(Arc::new(Mock::new(script)))
        .with_recorder(Arc::new(UsageRecorder::new(profile.store.clone(), prices)));
    let summarizer = AuxSummarizer::new(
        Arc::new(caller),
        aux,
        CallContext::new(s, "compression"),
        Some(16_000),
    );
    let model = mock_model(&profile, "[[rule]]\ntext = \"ok\"\n", "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |setup| {
        setup.summarizer = Some(Arc::new(summarizer));
    });

    let outcome = run
        .agent
        .compress(&profile.scope, &manual::parse("here 1").unwrap())
        .await
        .unwrap();

    assert_eq!(outcome.status, Status::Compressed);
    assert!(!outcome.fallback);
    let summary = active(&profile, s)[0].content.clone().unwrap();
    assert!(summary.contains("## Goal\nAUX-GOAL"));
    assert!(summary.contains("## Next Steps\nAUX-NEXT"));
    let purposes: Vec<String> = profile
        .store
        .usage_events(s)
        .unwrap()
        .into_iter()
        .map(|e| e.purpose)
        .collect();
    assert_eq!(purposes, ["auxiliary.compression"]);
}
