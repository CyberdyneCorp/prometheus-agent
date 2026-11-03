//! Plugin hooks (spec: plugin-system, Typed hook registry; Hook isolation
//! and time bounds; `pre_tool_call` directives; `pre_llm_call` context
//! preserves the prompt cache; Hook payloads are versioned and additive).

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Fake, HOOK, Profile};
use pmt_plugins::hooks::{PluginToolHooks, spill_dir};
use pmt_plugins::host::{CallInfo, Runtime};
use pmt_plugins::load::{Loaded, State};
use pmt_tools::dispatch::{HookCall, PreDecision, ToolHooks};
use pmt_tools::result::Status;
use serde_json::json;

/// Installs the `testing-hooks` plugin as `id` answering `mode` on `hooks`.
fn hook_plugin(profile: &mut Profile, fake: &Fake, id: &str, hooks: &str, mode: &str) {
    let declared: Vec<String> = hooks.split(',').map(|h| format!("\"{h}\"")).collect();
    profile.install(
        id,
        "testing-hooks",
        HOOK,
        &format!("hooks = [{}]", declared.join(", ")),
    );
    fake.set(id, "hooks", &format!("\"{hooks}\""));
    fake.set(id, "mode", &format!("\"{mode}\""));
    fake.set(id, "name", &format!("\"{id}\""));
}

fn session() -> CallInfo {
    CallInfo {
        session_id: Some("ses_test".to_owned()),
        surface: "cli".to_owned(),
        cwd: None,
    }
}

async fn pre_tool_call(loaded: &Loaded) -> PreDecision {
    let hooks = PluginToolHooks::new(Arc::clone(&loaded.hooks), session());
    let arguments = json!({"path": "a"});
    hooks
        .pre_tool_call(HookCall {
            session_id: "ses_test",
            call_id: "call_1",
            turn_id: None,
            tool: "write_file",
            arguments: &arguments,
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_hook_name_fails_at_load() {
    // Spec scenario "Typo is a load error", both where a typo can be: the
    // manifest's `hooks` capability and the component's subscriptions.
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    profile.place("typo", "testing-hooks", HOOK, "hooks = [\"pre_tool_cal\"]");
    profile.config.enabled.push("typo".to_owned());
    hook_plugin(&mut profile, &fake, "sly", "pre_tool_call", "none");
    fake.set("sly", "hooks", "\"pre_tool_cal\"");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &fake.ports());
    assert!(loaded.plugins.is_empty());
    for status in &loaded.statuses {
        assert_eq!(status.state, State::Error, "{status:?}");
        assert_eq!(
            status.problem.as_deref(),
            Some("unknown hook 'pre_tool_cal'")
        );
    }
    assert!(!loaded.hooks.handles("pre_tool_call"));
}

#[tokio::test(flavor = "multi_thread")]
async fn block_beats_approve_and_modify_merges() {
    // Spec scenario "Block beats approve": the approving plugin loads first.
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    hook_plugin(&mut profile, &fake, "a-approve", "pre_tool_call", "approve");
    hook_plugin(&mut profile, &fake, "b-block", "pre_tool_call", "block");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &fake.ports());
    assert_eq!(
        pre_tool_call(&loaded).await,
        PreDecision::Block("blocked by b-block".to_owned())
    );

    let mut profile = Profile::new("default");
    hook_plugin(&mut profile, &fake, "a-approve", "pre_tool_call", "approve");
    hook_plugin(
        &mut profile,
        &fake,
        "c-modify",
        "pre_tool_call",
        "modify:{'path':'b'}",
    );
    let loaded = profile.load(&runtime, &fake.ports());
    let PreDecision::Escalate(escalation) = pre_tool_call(&loaded).await else {
        panic!("approve beats modify");
    };
    assert_eq!(escalation.rule_key, "test-rule");
    assert_eq!(escalation.reason, "needs a look");

    let mut profile = Profile::new("default");
    hook_plugin(
        &mut profile,
        &fake,
        "c-modify",
        "pre_tool_call",
        "modify:{'path':'b'}",
    );
    let loaded = profile.load(&runtime, &fake.ports());
    let PreDecision::Modify(fields) = pre_tool_call(&loaded).await else {
        panic!("modify");
    };
    assert_eq!(fields["path"], "b");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_hook_does_not_block_the_next_one() {
    // Spec scenario "Failing hook does not block others".
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    hook_plugin(&mut profile, &fake, "a-fails", "post_tool_call", "error");
    hook_plugin(&mut profile, &fake, "b-records", "post_tool_call", "none");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &fake.ports());
    let hooks = PluginToolHooks::new(Arc::clone(&loaded.hooks), session());
    let arguments = json!({"path": "a"});
    let call = HookCall {
        session_id: "ses_test",
        call_id: "call_1",
        turn_id: None,
        tool: "read_file",
        arguments: &arguments,
    };
    hooks.post_tool_call(call, Status::Ok, 12).await;
    let seen = std::fs::read_to_string(
        profile
            .path()
            .join("plugin-data/b-records/kv")
            .join(hex("last")),
    )
    .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&seen).unwrap();
    assert_eq!(payload["tool"], "read_file");
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["duration_ms"], 12);
    assert_eq!(payload["schema_version"], "prometheus.hooks.v1");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_hook_counts_as_no_result_and_is_skipped_for_a_while() {
    let mut profile = Profile::new("default");
    profile.config.hook_timeout_ms = 200;
    let fake = Fake::new();
    hook_plugin(&mut profile, &fake, "a-slow", "pre_tool_call", "spin");
    hook_plugin(&mut profile, &fake, "b-block", "pre_tool_call", "block");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &fake.ports());

    let started = Instant::now();
    let decision = pre_tool_call(&loaded).await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        decision,
        PreDecision::Block("blocked by b-block".to_owned())
    );
    let started = Instant::now();
    pre_tool_call(&loaded).await;
    assert!(
        started.elapsed() < Duration::from_millis(150),
        "the suspended callback was skipped: {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn large_hook_context_is_spilled() {
    // Spec scenario "Large hook context is spilled".
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    hook_plugin(&mut profile, &fake, "big", "pre_llm_call", "big:50000");
    hook_plugin(
        &mut profile,
        &fake,
        "small",
        "pre_llm_call",
        "context:Answer briefly.",
    );
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &fake.ports());
    let context = loaded
        .hooks
        .pre_llm_call("ses_test", "hello", &session())
        .await
        .unwrap();
    assert!(context.ends_with("\n\nAnswer briefly."), "{context}");
    assert!(
        context.len() < 1300,
        "about 1000 characters: {}",
        context.len()
    );
    let dir = spill_dir(profile.path(), "ses_test");
    let spilled: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(spilled.len(), 1);
    assert!(context.contains(&spilled[0].display().to_string()));
    assert_eq!(
        std::fs::read_to_string(&spilled[0]).unwrap(),
        "x".repeat(50_000)
    );
}

fn hex(key: &str) -> String {
    use std::fmt::Write as _;
    key.bytes().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}
