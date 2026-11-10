//! The M2 compression upgrades (spec: context-compression): host
//! validation of engine output, gateway session hygiene, the summary
//! quality check, pinned messages, micro-compaction, prepared-ahead
//! summaries and the proactive prune, each on golden transcripts.

use std::sync::Arc;

use pmt_agent::compress::engine::{ContextEngine, Output};
use pmt_agent::compress::micro::{FOLDED_FLAG, ROLLING_FLAG};
use pmt_agent::compress::{Status, manual, pins};
use pmt_agent::tools::todo::TodoList;
use pmt_agent::turn::model::BoxFuture;
use pmt_store::{Message, NewMessage, Role, Source};
use serde_json::json;

use crate::compression::{
    FakeSummarizer, SMALL, active, committed, exchanges, failures, rebuilds, with_summarizer,
};
use crate::support::{FakeTools, Profile, agent, mock_model};

const OK: &str = "[[rule]]\ntext = \"ok\"\nrepeat = true\n";

fn texts(rows: &[Message]) -> Vec<String> {
    rows.iter()
        .map(|m| m.content.clone().unwrap_or_default())
        .collect()
}

fn summary_text(rows: &[Message]) -> String {
    rows.iter()
        .filter_map(|m| m.content.clone())
        .find(|t| t.starts_with(pmt_agent::compress::summary::HEADER))
        .expect("a summary row")
}

fn flagged(rows: &[Message], key: &str) -> usize {
    rows.iter()
        .filter(|m| m.metadata.as_ref().is_some_and(|v| v[key] == true))
        .count()
}

/// Returns two adjacent assistant rows in place of the transcript.
struct Breaking;

impl ContextEngine for Breaking {
    fn compact<'a>(&'a self, rows: &'a [Message]) -> BoxFuture<'a, Result<Vec<Output>, String>> {
        Box::pin(async move {
            let last = rows.last().unwrap().id;
            Ok(vec![
                Output::New(Box::new(NewMessage::new(Role::Assistant, "summary"))),
                Output::New(Box::new(NewMessage::new(Role::Assistant, "more summary"))),
                Output::Kept(last),
            ])
        })
    }
}

/// Scenario: Plugin engine breaks alternation (Engine output is validated
/// by the host).
#[tokio::test]
async fn a_plugin_engine_breaking_alternation_leaves_the_transcript_unchanged() {
    let profile = Profile::new(SMALL);
    let s = profile.session(Source::Cli);
    exchanges(&profile, s, 6, 400);
    let before = texts(&active(&profile, s));
    let model = mock_model(&profile, OK, "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |setup| {
        setup.context_engine = Some(Arc::new(Breaking));
    });
    let outcome = run
        .agent
        .compress(&profile.scope, &manual::parse("").unwrap())
        .await
        .unwrap();
    assert_eq!(outcome.status, Status::Failed);
    assert_eq!(texts(&active(&profile, s)), before);
    let failed = failures(&run);
    assert_eq!(failed[0].reason, "invalid_engine_output");
    assert_eq!(
        failed[0].message.as_deref(),
        Some("two adjacent assistant rows")
    );
    assert!(committed(&run).is_empty());
}

/// Scenario: Overnight growth (Gateway session hygiene).
#[tokio::test]
async fn gateway_hygiene_compacts_a_session_near_its_window_before_the_turn() {
    let profile = Profile::new(SMALL);
    let s = profile.session(Source::Gateway);
    // 30 exchanges of ~500 tokens: about 95% of the 16K window.
    exchanges(&profile, s, 30, 2_000);
    let model = mock_model(&profile, OK, "mock").await;
    let summarizer = FakeSummarizer::new();
    let run = agent(
        &profile,
        s,
        model,
        FakeTools::new(),
        with_summarizer(&summarizer),
    );

    let outcome = run.agent.gateway_hygiene(&profile.scope).await.unwrap();

    assert_eq!(outcome.unwrap().status, Status::Compressed);
    let done = committed(&run);
    assert_eq!(done[0].trigger, "gateway_hygiene");
    assert!(done[0].tokens_after < done[0].tokens_before);
    // Now well under 85%: the next message runs without another one.
    assert!(
        run.agent
            .gateway_hygiene(&profile.scope)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn gateway_hygiene_skips_small_and_disabled_sessions() {
    let profile = Profile::new(&format!("{SMALL}[compression]\nenabled = false\n"));
    let s = profile.session(Source::Gateway);
    exchanges(&profile, s, 30, 2_000);
    let model = mock_model(&profile, OK, "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    assert!(
        run.agent
            .gateway_hygiene(&profile.scope)
            .await
            .unwrap()
            .is_none()
    );

    let profile = Profile::new(SMALL);
    let s = profile.session(Source::Gateway);
    // Three huge messages: over the window, but under 4 messages.
    profile
        .store
        .append_messages(
            s,
            &[
                NewMessage::new(Role::User, "u".repeat(80_000)),
                NewMessage::new(Role::Assistant, "a"),
                NewMessage::new(Role::User, "again"),
            ],
        )
        .unwrap();
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    assert!(
        run.agent
            .gateway_hygiene(&profile.scope)
            .await
            .unwrap()
            .is_none()
    );
}

/// A region that patched `src/auth.rs`, before two plain exchanges.
fn patched_auth(profile: &Profile, s: pmt_core::id::SessionId) {
    let mut call = NewMessage::new(Role::Assistant, "");
    let patch = "*** Begin Patch\n*** Update File: src/auth.rs\n@@\n-a\n+b\n*** End Patch";
    call.tool_calls = Some(json!([
        {"id": "c1", "name": "patch", "arguments": json!({"patch": patch}).to_string()}
    ]));
    let mut result = NewMessage::new(Role::Tool, "patched");
    result.tool_call_id = Some("c1".into());
    result.tool_name = Some("patch".into());
    profile
        .store
        .append_messages(
            s,
            &[
                NewMessage::new(Role::User, "fix the login"),
                call,
                result,
                NewMessage::new(Role::Assistant, format!("done {}", "d".repeat(2_000))),
            ],
        )
        .unwrap();
    exchanges(profile, s, 2, 400);
}

/// Scenario: Summary forgot a file (Summary quality check).
#[tokio::test]
async fn a_file_the_summary_forgot_is_preserved_and_the_coverage_recorded() {
    let profile = Profile::new(SMALL);
    let s = profile.session(Source::Cli);
    patched_auth(&profile, s);
    let model = mock_model(&profile, OK, "mock").await;
    let summarizer = FakeSummarizer::new();
    for _ in 0..2 {
        summarizer
            .script
            .lock()
            .unwrap()
            .push_back(Ok("## Goal\nFix the login.".into()));
    }
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
    // Below the coverage floor: asked once more, naming what was missing.
    let asked = summarizer.texts();
    assert_eq!(asked.len(), 2);
    assert!(asked[1].contains("LEFT THESE OUT"));
    assert!(asked[1].contains("- src/auth.rs"));
    let summary = summary_text(&active(&profile, s));
    let preserved = summary.split("## Preserved References").nth(1).unwrap();
    assert!(preserved.contains("- src/auth.rs"));
    let coverage = committed(&run)[0].coverage.unwrap();
    assert!(coverage < 0.8, "{coverage}");
}

/// Scenario: Open todos survive verbatim (Summary quality check).
#[tokio::test]
async fn open_todos_are_kept_verbatim_in_the_summary() {
    let profile = Profile::new(SMALL);
    let s = profile.session(Source::Cli);
    exchanges(&profile, s, 6, 400);
    let mut todos = TodoList::default();
    todos
        .call(json!({"todos": [
            {"id": "t1", "content": "migrate the users table", "status": "pending"},
            {"id": "t2", "content": "update the API docs", "status": "pending"},
            {"id": "t3", "content": "old", "status": "completed"},
        ]}))
        .unwrap();
    todos.save(&profile.store, s).unwrap();
    let model = mock_model(&profile, OK, "mock").await;
    let summarizer = FakeSummarizer::new();
    let run = agent(
        &profile,
        s,
        model,
        FakeTools::new(),
        with_summarizer(&summarizer),
    );

    run.agent
        .compress(&profile.scope, &manual::parse("here 1").unwrap())
        .await
        .unwrap();

    let summary = summary_text(&active(&profile, s));
    let open = summary
        .split("## Open Todos\n")
        .nth(1)
        .unwrap()
        .split("\n## ")
        .next()
        .unwrap();
    assert!(open.contains("- [pending] migrate the users table (t1)"));
    assert!(open.contains("- [pending] update the API docs (t2)"));
    assert!(!open.contains("old"));
}

/// Scenario: Pinned requirement kept (Pinned messages survive compaction).
#[tokio::test]
async fn a_pinned_message_is_in_the_summary_after_three_compactions() {
    let profile = Profile::new(SMALL);
    let s = profile.session(Source::Cli);
    profile
        .store
        .append_message(
            s,
            &NewMessage::new(Role::User, "never touch the prod database"),
        )
        .unwrap();
    profile
        .store
        .append_message(s, &NewMessage::new(Role::Assistant, "understood"))
        .unwrap();
    let pinned = pins::pin(&profile.store, s, "", 8_000).unwrap();
    assert_eq!(pinned.text, "understood");
    pins::unpin(&profile.store, s, &pinned.id.to_string()[..20]).unwrap();
    let user = active(&profile, s)[0].id.to_string();
    pins::pin(&profile.store, s, &user, 8_000).unwrap();
    let model = mock_model(&profile, OK, "mock").await;
    let summarizer = FakeSummarizer::new();
    let run = agent(
        &profile,
        s,
        model,
        FakeTools::new(),
        with_summarizer(&summarizer),
    );

    for _ in 0..3 {
        exchanges(&profile, s, 4, 400);
        let outcome = run
            .agent
            .compress(&profile.scope, &manual::parse("here 1").unwrap())
            .await
            .unwrap();
        assert_eq!(outcome.status, Status::Compressed, "{:?}", failures(&run));
    }

    let summary = summary_text(&active(&profile, s));
    let section = summary.split("## Pinned\n").nth(1).unwrap();
    assert!(section.starts_with("never touch the prod database\n"));
    assert_eq!(committed(&run).last().unwrap().epoch, 3);
}

#[tokio::test]
async fn a_pin_over_the_budget_is_refused() {
    let profile = Profile::new(SMALL);
    let s = profile.session(Source::Cli);
    profile
        .store
        .append_messages(
            s,
            &[
                NewMessage::new(Role::User, "p".repeat(400)),
                NewMessage::new(Role::Assistant, "q".repeat(400)),
            ],
        )
        .unwrap();
    let rows = active(&profile, s);
    pins::pin(&profile.store, s, &rows[0].id.to_string(), 150).unwrap();
    let err = pins::pin(&profile.store, s, "last", 150).unwrap_err();
    assert_eq!(
        err.message(),
        "pinned content would exceed 150 tokens; unpin something first"
    );
    assert_eq!(err.code().code, "compression.pin_limit");
    assert!(pins::unpin(&profile.store, s, "msg_nope").is_err());
}

/// Scenario: Off by default (Opt-in micro-compaction).
#[tokio::test]
async fn micro_compaction_is_off_by_default() {
    let profile = Profile::new(SMALL);
    let s = profile.session(Source::Cli);
    exchanges(&profile, s, 12, 1_000);
    let model = mock_model(&profile, OK, "mock").await;
    let summarizer = FakeSummarizer::new();
    let run = agent(
        &profile,
        s,
        model,
        FakeTools::new(),
        with_summarizer(&summarizer),
    );
    for text in ["one", "two"] {
        run.turn(&profile, text).await;
    }
    let rows = active(&profile, s);
    assert_eq!(flagged(&rows, ROLLING_FLAG), 0);
    assert!(summarizer.texts().is_empty());
}

/// Scenario: User messages preserved (Opt-in micro-compaction).
#[tokio::test]
async fn micro_compaction_folds_one_exchange_per_turn_and_keeps_every_user_message() {
    let profile = Profile::new(&format!("{SMALL}[compression]\nmicro_compact = true\n"));
    let s = profile.session(Source::Cli);
    // Longer than the protected tail: the middle holds whole exchanges.
    exchanges(&profile, s, 12, 2_000);
    let users_before: Vec<String> = active(&profile, s)
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| m.content.clone().unwrap())
        .collect();
    let model = mock_model(&profile, OK, "mock").await;
    let summarizer = FakeSummarizer::new();
    let run = agent(
        &profile,
        s,
        model,
        FakeTools::new(),
        with_summarizer(&summarizer),
    );

    for text in ["one", "two", "three", "four"] {
        run.turn(&profile, text).await;
    }

    let rows = active(&profile, s);
    // Four passes, four folds: one rolling row, three former ones.
    assert_eq!(summarizer.texts().len(), 4);
    assert_eq!(flagged(&rows, ROLLING_FLAG), 1);
    assert_eq!(flagged(&rows, FOLDED_FLAG), 3);
    let users: Vec<String> = rows
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| m.content.clone().unwrap())
        .collect();
    let mut expected = users_before;
    expected.extend(["one", "two", "three", "four"].map(String::from));
    assert_eq!(users, expected);
    // The second pass updated the first rolling summary.
    assert!(summarizer.texts()[1].contains("ROLLING SUMMARY SO FAR"));
    assert!(rows.windows(2).all(|w| w[0].role != w[1].role));
}

/// Scenario: Prepared summary adopted (Prepared-ahead summaries).
#[tokio::test]
async fn a_summary_prepared_near_the_trigger_is_adopted_without_another_call() {
    let mut profile = Profile::new(SMALL);
    let s = profile.session(Source::Cli);
    exchanges(&profile, s, 10, 2_000);
    let model = mock_model(&profile, OK, "mock").await;
    let summarizer = FakeSummarizer::new();
    let run = agent(
        &profile,
        s,
        model.clone(),
        FakeTools::new(),
        with_summarizer(&summarizer),
    );
    let first = run.turn(&profile, "hello").await;
    // The trigger just above the reported size: about 95% of it.
    let usage = first.usage;
    let reported = usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens;
    let trigger = reported * 100 / 95;
    profile.reconfigure(&format!(
        "{SMALL}[compression]\nthreshold_tokens = {trigger}\nprepare_ahead = true\n"
    ));

    run.turn(&profile, "again").await;
    assert!(committed(&run).is_empty());
    // The background task gets its turn on the test's runtime.
    for _ in 0..100 {
        if !summarizer.texts().is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(summarizer.texts().len(), 1, "prepared in the background");
    // Only new rows since: the next turn crosses the trigger and adopts it.
    run.turn(&profile, &format!("big {}", "b".repeat(2_000)))
        .await;

    let done = committed(&run);
    assert_eq!(done.len(), 1, "{:?}", failures(&run));
    assert_eq!(summarizer.texts().len(), 1, "no new summarizer call");
}

/// Old exchanges with two 10,000-character tool results, then plain ones.
fn large_tool_results(profile: &Profile, s: pmt_core::id::SessionId) {
    // The protected head (first three rows) comes first.
    exchanges(profile, s, 2, 10);
    for id in ["c1", "c2"] {
        let mut call = NewMessage::new(Role::Assistant, "");
        call.tool_calls = Some(json!([
            {"id": id, "name": "terminal", "arguments": "{\"command\":\"make\"}"}
        ]));
        let mut result = NewMessage::new(Role::Tool, format!("{id} {}", "l".repeat(10_000)));
        result.tool_call_id = Some(id.into());
        result.tool_name = Some("terminal".into());
        profile
            .store
            .append_messages(
                s,
                &[
                    NewMessage::new(Role::User, format!("build {id}")),
                    call,
                    result,
                    NewMessage::new(Role::Assistant, "built"),
                ],
            )
            .unwrap();
    }
    exchanges(profile, s, 6, 1_000);
}

fn tool_texts(profile: &Profile, s: pmt_core::id::SessionId) -> Vec<String> {
    active(profile, s)
        .into_iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.content.unwrap_or_default())
        .collect()
}

/// Scenario: Disabled by default (Opt-in proactive prune).
#[tokio::test]
async fn no_proactive_prune_runs_by_default() {
    let profile = Profile::new(SMALL);
    let s = profile.session(Source::Cli);
    large_tool_results(&profile, s);
    let model = mock_model(&profile, OK, "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    for text in ["one", "two"] {
        run.turn(&profile, text).await;
    }
    assert!(tool_texts(&profile, s).iter().all(|t| t.len() > 10_000));
    // Its rows were stored before its first prompt ("missing").
    assert_eq!(rebuilds(&run), ["missing"]);
}

#[tokio::test]
async fn a_proactive_prune_shrinks_large_old_results_once_past_its_size() {
    let profile = Profile::new(&format!(
        "{SMALL}[compression]\nproactive_prune_tokens = 4000\n"
    ));
    let s = profile.session(Source::Cli);
    large_tool_results(&profile, s);
    let model = mock_model(&profile, OK, "mock").await;
    let run = agent(&profile, s, model, FakeTools::new(), |_| {});
    for text in ["one", "two", "three"] {
        run.turn(&profile, text).await;
    }
    let results = tool_texts(&profile, s);
    assert!(
        results
            .iter()
            .all(|t| t.starts_with("[pruned tool result] terminal(")),
        "{results:?}"
    );
    // One commit, a sanctioned cache break; not rearmed since.
    assert_eq!(rebuilds(&run), ["missing", "compaction"]);
    assert!(committed(&run).is_empty());
}
