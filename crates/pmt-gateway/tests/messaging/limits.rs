//! Per-user rate limits and per-chat policies (spec: messaging-gateway,
//! Per-user rate limiting; Per-chat policies).

use std::time::Duration;

use jiff::Timestamp;
use pmt_gateway::messaging::{Admission, ChatType, Inbound};
use pmt_store::{CostStatus, Tokens, UsageEvent};

use crate::support::{Harness, Setup, config, eventually, open, said};

#[tokio::test]
async fn a_burst_beyond_the_limit_is_dropped_with_one_slow_down_reply() {
    let h = Harness::start(Setup::new(&config(
        "telegram",
        "allow_all = true\nadmins = [\"admin\"]",
    )))
    .await;
    let mut admitted = 0;
    let mut limited = 0;
    // 40 messages in about 10 seconds' worth of sending; with the
    // defaults (burst 10, 20/minute) only the burst and its refill pass.
    for i in 0..40 {
        match h.dm("telegram", "222", &format!("message {i}")).await {
            Admission::RateLimited => limited += 1,
            _ => admitted += 1,
        }
    }
    assert!((10..=13).contains(&admitted), "admitted {admitted}");
    assert_eq!(admitted + limited, 40);
    said(
        &h.adapter("telegram"),
        "Slow down: rate limit reached, try again in",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let notices = h
        .adapter("telegram")
        .texts()
        .iter()
        .filter(|t| t.starts_with("Slow down"))
        .count();
    assert_eq!(notices, 1, "one slow-down reply per window");
}

#[tokio::test]
async fn admins_are_exempt_from_rate_limits() {
    let h = Harness::start(Setup::new(&config(
        "telegram",
        "allow_all = true\nadmins = [\"admin\"]\n[gateway.platforms.telegram.rate_limit]\nburst = 1\nmessages_per_minute = 1",
    )))
    .await;
    for i in 0..5 {
        assert_ne!(
            h.dm("telegram", "admin", &format!("hi {i}")).await,
            Admission::RateLimited
        );
    }
    assert_ne!(h.dm("telegram", "222", "one").await, Admission::RateLimited);
    assert_eq!(h.dm("telegram", "222", "two").await, Admission::RateLimited);
}

#[tokio::test]
async fn a_spent_daily_budget_starts_no_turn() {
    let h = Harness::start(Setup::new(&config(
        "telegram",
        "allow_all = true\n[gateway.platforms.telegram.rate_limit]\ndaily_cost_usd = 2.0",
    )))
    .await;
    h.dm("telegram", "u1", "first").await;
    said(&h.adapter("telegram"), "echo: first").await;
    let session = h.sessions()[0];
    h.store()
        .record_usage(&UsageEvent {
            call_id: "c1".into(),
            ts: Timestamp::now(),
            session_id: session,
            turn_id: None,
            parent_call_id: None,
            purpose: "main".into(),
            purpose_detail: None,
            provider: "openai".into(),
            model: "gpt".into(),
            service_tier: None,
            tokens: Tokens::default(),
            cost_usd: Some(2.05),
            cost_status: CostStatus::Exact,
            billing_source: "api_key:openai".into(),
            request_hash: None,
            cache_hit: false,
        })
        .unwrap();
    assert_eq!(
        h.dm("telegram", "u1", "summarize my inbox").await,
        Admission::RateLimited
    );
    said(&h.adapter("telegram"), "it resets at local midnight").await;
    assert_eq!(h.agents.texts(), vec!["first"], "no turn started");
    assert_ne!(
        h.dm("telegram", "u2", "another user").await,
        Admission::RateLimited
    );
}

/// Spec: the daily cap is keyed by profile and canonical sender. In a
/// group session shared by everyone, the turn that spent is charged to its
/// sender, not to whoever started the session.
#[tokio::test]
async fn in_a_shared_session_each_sender_pays_for_their_own_turns() {
    let h = Harness::start(Setup::new(&config(
        "telegram",
        "allow_all = true\n[gateway.platforms.telegram.sessions]\ngroup_per_user = false\n[gateway.platforms.telegram.rate_limit]\ndaily_cost_usd = 2.0",
    )))
    .await;
    let group = |sender: &str, text: &str| Inbound::text(ChatType::Group, "g1", sender, text);
    h.send("telegram", group("A", "hello")).await;
    said(&h.adapter("telegram"), "echo: hello").await;
    h.send("telegram", group("B", "expensive")).await;
    said(&h.adapter("telegram"), "echo: expensive").await;
    assert_eq!(h.sessions().len(), 1, "one shared session");
    h.store()
        .record_usage(&UsageEvent {
            call_id: "b-call".into(),
            ts: Timestamp::now(),
            session_id: h.sessions()[0],
            turn_id: None,
            parent_call_id: None,
            purpose: "main".into(),
            purpose_detail: None,
            provider: "openai".into(),
            model: "gpt".into(),
            service_tier: None,
            tokens: Tokens::default(),
            cost_usd: Some(2.05),
            cost_status: CostStatus::Exact,
            billing_source: "api_key:openai".into(),
            request_hash: None,
            cache_hit: false,
        })
        .unwrap();
    assert_ne!(
        h.send("telegram", group("A", "still mine")).await,
        Admission::RateLimited,
        "B's spend is not A's"
    );
    said(&h.adapter("telegram"), "echo: still mine").await;
    assert_eq!(
        h.send("telegram", group("B", "more")).await,
        Admission::RateLimited
    );
}

fn policy_config(model: &str) -> String {
    format!(
        "[tools]\nenabled_toolsets = [\"web\", \"file\"]\n\n{}\n[[gateway.chat_policies]]\nplatform = \"discord\"\nchat_id = \"555\"\nmodel = \"{model}\"\ntoolsets = [\"web\"]\n",
        open("discord")
    )
}

#[tokio::test]
async fn a_matched_chat_gets_the_policy_frozen_with_its_session() {
    let h = Harness::start(Setup::new(&policy_config("model-a"))).await;
    h.send(
        "discord",
        Inbound::text(ChatType::Group, "555", "u1", "hello"),
    )
    .await;
    said(&h.adapter("discord"), "echo: hello").await;
    h.send(
        "discord",
        Inbound::text(ChatType::Group, "777", "u1", "elsewhere"),
    )
    .await;
    said(&h.adapter("discord"), "echo: elsewhere").await;
    let conversations = h.agents.conversations.lock().unwrap().clone();
    let matched = conversations.iter().find(|c| c.chat_id == "555").unwrap();
    let policy = matched.policy.as_ref().expect("the policy applies");
    assert_eq!(policy.toolsets.as_deref(), Some(&["web".to_owned()][..]));
    let other = conversations.iter().find(|c| c.chat_id == "777").unwrap();
    assert!(other.policy.is_none());
}

#[tokio::test]
async fn a_policy_edit_reaches_the_next_session_only() {
    let h = Harness::start(Setup::new(&policy_config("model-a"))).await;
    let group = |text: &str| Inbound::text(ChatType::Group, "555", "u1", text);
    h.send("discord", group("one")).await;
    said(&h.adapter("discord"), "echo: one").await;
    let first = h.sessions()[0];
    let frozen = |id| h.store().session_policy(id).unwrap().unwrap();
    assert!(frozen(first).contains("model-a"));
    // The policy's model changes; the daemon reloads the config.
    let config = h.home.path().join("profiles/default/config.toml");
    std::fs::write(&config, policy_config("model-b")).unwrap();
    h.send("discord", group("two")).await;
    said(&h.adapter("discord"), "echo: two").await;
    assert_eq!(h.sessions().len(), 1, "the session continues");
    assert!(frozen(first).contains("model-a"), "and keeps its policy");
    h.send("discord", group("/new")).await;
    said(&h.adapter("discord"), "New session started.").await;
    h.send("discord", group("three")).await;
    said(&h.adapter("discord"), "echo: three").await;
    let newest = h
        .sessions()
        .into_iter()
        .find(|id| *id != first)
        .expect("a new session");
    assert!(frozen(newest).contains("model-b"));
}

#[tokio::test]
async fn policy_apply_now_replaces_the_session_for_admins() {
    let mut text = policy_config("model-a");
    text = text.replace(
        "allow_all = true",
        "allow_all = true\ngroup_admins = [\"boss\"]",
    );
    let h = Harness::start(Setup::new(&text)).await;
    let from = |sender: &str, text: &str| Inbound::text(ChatType::Group, "555", sender, text);
    h.send("discord", from("boss", "hi")).await;
    said(&h.adapter("discord"), "echo: hi").await;
    h.send("discord", from("u1", "/policy apply --now")).await;
    said(
        &h.adapter("discord"),
        "This command is restricted to admins.",
    )
    .await;
    h.send("discord", from("boss", "/policy apply --now")).await;
    said(
        &h.adapter("discord"),
        "New session started with the current policy.",
    )
    .await;
    eventually("a second session", || h.sessions().len() == 2).await;
    let newest = h.sessions()[0];
    let routed = h.store().routed_session(newest).unwrap();
    let reasons: Vec<_> = h
        .sessions()
        .into_iter()
        .filter_map(|id| h.store().routed_session(id).unwrap())
        .filter_map(|r| r.reset_reason)
        .collect();
    assert!(
        reasons.contains(&pmt_store::gateway::routes::ResetReason::Policy),
        "{reasons:?} {routed:?}"
    );
}
