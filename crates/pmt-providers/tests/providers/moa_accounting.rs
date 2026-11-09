//! Mixture of Agents accounting (spec: mixture-of-agents, MoA billing and
//! accounting; MoA traces; Privacy filter for advisor outputs; MoA as a
//! fallback entry).

use std::sync::Arc;

use pmt_core::secret::Redactor;
use pmt_obs::usage::UsageRecorder;
use pmt_providers::moa::Mixture;
use pmt_store::{NewSession, Source};
use wiremock::{MockServer, ResponseTemplate};

use crate::caching::store;
use crate::moa::{call, chat_text, config, posted, respond, turn_ctx};
use crate::support::{Env, caller, request, sse};

/// Scenario: Ledger rows per slot.
#[tokio::test]
async fn every_slot_is_its_own_ledger_row() {
    let server = MockServer::start().await;
    respond(&server, "ra", chat_text("a")).await;
    respond(&server, "rb", chat_text("b")).await;
    respond(&server, "agg", sse("chat_tool_call.sse")).await;
    let env = Env::new(&config(&server.uri(), "", ""), &[]);
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let store = store(&dir.path().join("home"));
    let session = store
        .create_session(&NewSession::new(Source::Cli))
        .unwrap()
        .id;
    let catalog = Arc::new(pmt_providers::metadata::Catalog::new(&env.config, None));
    let caller = caller().with_recorder(Arc::new(UsageRecorder::new(store.clone(), catalog)));
    let outcome = call(
        &env,
        &caller,
        ("moa", "review"),
        &request("x"),
        Some(&turn_ctx(session, "t1")),
    )
    .await
    .unwrap();
    let mut rows: Vec<(String, String, String, Option<String>)> = store
        .usage_events(session)
        .unwrap()
        .into_iter()
        .map(|e| (e.purpose, e.provider, e.model, e.purpose_detail))
        .collect();
    rows.sort();
    let row = |purpose: &str, provider: &str| {
        (
            purpose.to_owned(),
            provider.to_owned(),
            "m".to_owned(),
            Some("review".to_owned()),
        )
    };
    assert_eq!(
        rows,
        [
            row("moa.aggregator", "agg"),
            row("moa.reference", "ra"),
            row("moa.reference", "rb"),
        ]
    );
    // The call's own record is the aggregator's row, billed to it.
    let record = outcome.stream.recorded().unwrap().as_ref().unwrap();
    assert_eq!(record.purpose, "moa.aggregator");
    // A keyless loopback aggregator bills as `local`.
    assert_eq!(record.billing_source, "local");
    assert_eq!(outcome.route.billing_source(), "local");
}

/// Scenario: Trace written.
#[tokio::test]
async fn a_trace_record_lists_every_slot_with_usage() {
    let server = MockServer::start().await;
    respond(&server, "ra", chat_text("a")).await;
    respond(&server, "rb", chat_text("b")).await;
    respond(&server, "agg", sse("chat_tool_call.sse")).await;
    let env = Env::new(
        &config(&server.uri(), "", "[moa]\nsave_traces = true\n"),
        &[],
    );
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mixture = Mixture::new(Some(dir.path().to_path_buf()), Redactor::default());
    let caller = caller().with_mixture(Arc::new(mixture));
    let ctx = turn_ctx(pmt_core::id::SessionId::new(), "t1");
    call(&env, &caller, ("moa", "review"), &request("x"), Some(&ctx))
        .await
        .unwrap();
    let file = dir
        .path()
        .join("moa-traces")
        .join(format!("{}.jsonl", ctx.session_id));
    let text = std::fs::read_to_string(&file).unwrap();
    let records: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record["preset"], "review");
    assert_eq!(record["iteration"], 1);
    let references = record["references"].as_array().unwrap();
    assert_eq!(references.len(), 2);
    assert_eq!(references[0]["output"], "a");
    assert_eq!(references[0]["request"]["tools"], 0);
    assert_eq!(references[0]["usage"]["output_tokens"], 5);
    let aggregator = &record["aggregator"];
    assert_eq!(aggregator["provider"], "agg");
    assert_eq!(aggregator["output"], "Reading the file.");
    assert_eq!(aggregator["tool_calls"], serde_json::json!(["read_file"]));
    assert_eq!(aggregator["usage"]["output_tokens"], 42);
    assert_eq!(aggregator["request"]["tools"], 1);
}

/// Scenario: Display-only redaction.
#[tokio::test]
async fn display_redaction_leaves_the_aggregator_the_raw_text() {
    let server = MockServer::start().await;
    respond(&server, "ra", chat_text("ask alice@example.com")).await;
    respond(&server, "rb", chat_text("call 555-123-4567")).await;
    respond(&server, "agg", sse("chat_text.sse")).await;
    let env = Env::new(
        &config(
            &server.uri(),
            "",
            "[moa]\nprivacy_filter = \"display\"\nsave_traces = true\n",
        ),
        &[],
    );
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mixture = Mixture::new(Some(dir.path().to_path_buf()), Redactor::default());
    let caller = caller().with_mixture(Arc::new(mixture));
    let ctx = turn_ctx(pmt_core::id::SessionId::new(), "t1");
    let outcome = call(&env, &caller, ("moa", "review"), &request("x"), Some(&ctx))
        .await
        .unwrap();
    let shown = outcome.advice.join("\n");
    assert!(!shown.contains("alice@example.com"), "{shown}");
    assert!(shown.contains("[REDACTED:email]"), "{shown}");
    assert!(!shown.contains("555-123-4567"), "{shown}");
    let sent = posted(&server, "/agg/").await[0].to_string();
    assert!(
        sent.contains("alice@example.com"),
        "the aggregator reads raw text"
    );
    assert!(sent.contains("555-123-4567"));
    let trace = std::fs::read_to_string(
        dir.path()
            .join("moa-traces")
            .join(format!("{}.jsonl", ctx.session_id)),
    )
    .unwrap();
    assert!(!trace.contains("alice@example.com"), "{trace}");
}

/// Scenario: SHA untouched.
#[tokio::test]
async fn the_full_filter_redacts_guidance_but_not_a_commit_sha() {
    let server = MockServer::start().await;
    respond(&server, "ra", chat_text("revert commit a1b2c3d4e5f6")).await;
    respond(&server, "rb", chat_text("mail bob@example.org")).await;
    respond(&server, "agg", sse("chat_text.sse")).await;
    let env = Env::new(
        &config(&server.uri(), "", "[moa]\nprivacy_filter = \"full\"\n"),
        &[],
    );
    call(&env, &caller(), ("moa", "review"), &request("x"), None)
        .await
        .unwrap();
    let sent = posted(&server, "/agg/").await[0].to_string();
    assert!(sent.contains("revert commit a1b2c3d4e5f6"), "{sent}");
    assert!(!sent.contains("bob@example.org"), "{sent}");
}

/// Scenario: Preset activated on primary failure.
#[tokio::test]
async fn a_preset_in_the_chain_takes_over_from_an_overloaded_primary() {
    let server = MockServer::start().await;
    respond(&server, "prim", ResponseTemplate::new(529)).await;
    respond(&server, "ra", chat_text("a")).await;
    respond(&server, "rb", chat_text("b")).await;
    respond(&server, "agg", sse("chat_text.sse")).await;
    // `broken`'s aggregator has no credential: the entry is skipped.
    let chain = "[moa.presets.broken]\nreference_models = [{provider = \"ra\", model = \"m\"}]\n\
                 aggregator = {provider = \"openrouter\", model = \"x\"}\n\
                 [[fallback.chain]]\nprovider = \"moa\"\nmodel = \"broken\"\n\
                 [[fallback.chain]]\nprovider = \"moa\"\nmodel = \"review\"\n";
    let env = Env::new(&config(&server.uri(), "", chain), &[]);
    let outcome = call(&env, &caller(), ("prim", "m"), &request("x"), None)
        .await
        .unwrap();
    assert_eq!(outcome.route.label(), "moa/review");
    let fallback = outcome.fallbacks[0].payload();
    assert_eq!(fallback.from, "prim/m");
    assert_eq!(fallback.to, "moa/review");
    assert_eq!(fallback.reason, "overloaded");
    assert_eq!(posted(&server, "/ra/").await.len(), 1);
    assert_eq!(posted(&server, "/rb/").await.len(), 1);
    assert_eq!(posted(&server, "/agg/").await.len(), 1);
}
