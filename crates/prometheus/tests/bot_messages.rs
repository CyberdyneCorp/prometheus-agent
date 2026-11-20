//! Bots messaging bots (spec: bot-mode, `message_agent` tool; Typed delivery
//! failures and safe retry; Mentions from any chat; Bot protocol in
//! canonical Bot Chats; Bot Mode off switch; desktop-app, Agent-to-agent
//! messages from bot chats): several bots of one home whose mock models
//! call `message_agent`, through the real daemon or headlessly.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;

use pmt_store::bot_messages::Direction;
use serde_json::json;

use self::common::bots::{self, bot, bot_chat, call, client, said, store, tell, wait_said};
use self::common::{daemon, ok, run};

/// A tool call of `message_agent` to `target`, as a mock rule.
fn ask(user: &str, target: &str, message: &str) -> String {
    format!(
        "[[rule]]\nuser = {user:?}\n[[rule.tool_calls]]\nname = \"message_agent\"\n\
         arguments = {{ target = {target:?}, message = {message:?} }}\n"
    )
}

/// The rule a sender ends its tool round with (once: a tool result stays
/// the latest batch the mock sees in later turns too).
const SENT: &str = "[[rule]]\ntool_result = \"queued\"\ntext = \"message sent\"\n";

/// The `in` record of the only delivery `profile` received.
fn received(home: &Path, profile: &str) -> pmt_store::bot_messages::BotMessage {
    let store = store(home, profile);
    let mut all = Vec::new();
    for state in ["queued", "running", "delivered", "failed"] {
        all.extend(store.bot_messages_in(Direction::In, state).unwrap());
    }
    assert_eq!(all.len(), 1, "{all:?}");
    all.remove(0)
}

/// Scenarios: Hidden bot still reachable; the reply comes back to the
/// sender's Bot Chat; the recipient's copy is attributed.
#[test]
fn a_message_reaches_a_hidden_bot_and_its_reply_comes_back() {
    let rules = format!(
        "[[rule]]\nuser = \"Reply from bob\"\ntext = \"bob says 4\"\n{SENT}{}",
        ask("ask bob", "bob", "what is 2+2?")
    );
    let home = bots::home("[[rule]]\nrepeat = true\ntext = \"hi\"\n");
    bot(home.path(), "alice", &rules, &[]);
    bot(
        home.path(),
        "bob",
        "[[rule]]\nuser = \"Message from 🤖 Alice A (@alice)\"\ntext = \"4\"\n",
        &[],
    );
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let c = client(home.path(), &["default", "alice", "bob"]).await;
        call(
            &c,
            "bot.meta.update",
            json!({"profile": "alice", "changes": {"title": "Alice A"}}),
        )
        .await;
        call(
            &c,
            "bot.meta.update",
            json!({"profile": "bob", "changes": {"hidden": true}}),
        )
        .await;
        tell(&c, "alice", "ask bob").await;
    });
    wait_said(home.path(), "alice", "assistant", "bob says 4");
    let bob_chat = bot_chat(home.path(), "bob").unwrap();
    let incoming: Vec<String> = said(home.path(), "bob", &bob_chat, "user")
        .into_iter()
        .filter(|t| t.starts_with("Message from 🤖 Alice A (@alice)"))
        .collect();
    assert_eq!(incoming.len(), 1, "{incoming:?}");
    assert!(incoming[0].contains("what is 2+2?"));
    let row = received(home.path(), "bob");
    assert_eq!(
        (row.state.as_str(), row.reply.as_deref()),
        ("delivered", Some("4"))
    );
}

/// Scenario: Busy target.
#[test]
fn a_target_busy_past_the_wait_fails_with_target_busy() {
    let rules = format!(
        "{SENT}{}[[rule]]\nuser = \"target_busy\"\ntext = \"bob is busy\"\n",
        ask("ask bob", "bob", "anything new?")
    );
    let home = bots::home("[[rule]]\nrepeat = true\ntext = \"hi\"\n");
    bot(home.path(), "alice", &rules, &[]);
    bot(
        home.path(),
        "bob",
        "[[rule]]\nuser = \"long job\"\nlatency_ms = 20000\ntext = \"done\"\n",
        &[("bot_mode.turn_wait_secs", "2")],
    );
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let c = client(home.path(), &["default", "alice", "bob"]).await;
        tell(&c, "bob", "long job").await;
        tell(&c, "alice", "ask bob").await;
    });
    let told = wait_said(home.path(), "alice", "user", "[reason: target_busy]");
    assert!(
        told.iter()
            .any(|t| t.contains("failed [reason: target_busy]")),
        "{told:?}"
    );
    assert_eq!(
        received(home.path(), "bob").reason.as_deref(),
        Some("target_busy")
    );
}

/// Scenario: Quota failure surfaces at once.
#[test]
fn a_quota_failure_reaches_the_sender_without_a_retry() {
    let rules = format!(
        "{SENT}{}[[rule]]\nuser = \"provider_quota_limit\"\ntext = \"out of quota\"\n",
        ask("ask bob", "bob", "summarize the news")
    );
    let home = bots::home("[[rule]]\nrepeat = true\ntext = \"hi\"\n");
    bot(home.path(), "alice", &rules, &[]);
    bot(
        home.path(),
        "bob",
        "[[rule]]\nrepeat = true\nfault = \"quota\"\n",
        &[],
    );
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let c = client(home.path(), &["default", "alice", "bob"]).await;
        tell(&c, "alice", "ask bob").await;
    });
    wait_said(
        home.path(),
        "alice",
        "user",
        "[reason: provider_quota_limit]",
    );
    let row = received(home.path(), "bob");
    assert_eq!(row.reason.as_deref(), Some("provider_quota_limit"));
    assert_eq!(row.attempts, 1, "no retry");
}

/// Scenario: Retry keeps one copy.
#[test]
fn a_server_error_is_retried_once_and_the_message_is_kept_once() {
    let rules = format!(
        "[[rule]]\nuser = \"Reply from bob\"\ntext = \"bob answered\"\n{SENT}{}",
        ask("ask bob", "bob", "status?")
    );
    let home = bots::home("[[rule]]\nrepeat = true\ntext = \"hi\"\n");
    bot(home.path(), "alice", &rules, &[]);
    bot(
        home.path(),
        "bob",
        "[[rule]]\nfault = \"server_error\"\n[[rule]]\ntext = \"all green\"\n",
        &[
            ("agent.api_max_retries", "1"),
            ("agent.auto_recovery_cycles", "0"),
            ("providers.mock.max_retries", "0"),
        ],
    );
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let c = client(home.path(), &["default", "alice", "bob"]).await;
        tell(&c, "alice", "ask bob").await;
    });
    wait_said(home.path(), "alice", "assistant", "bob answered");
    let row = received(home.path(), "bob");
    assert_eq!((row.state.as_str(), row.attempts), ("delivered", 2));
    let chat = bot_chat(home.path(), "bob").unwrap();
    let copies = said(home.path(), "bob", &chat, "user")
        .iter()
        .filter(|t| t.contains("status?"))
        .count();
    assert_eq!(copies, 1);
}

/// Scenario: Disabled bot not messageable; the off switch also takes it
/// off the roster.
#[test]
fn a_bot_with_bot_mode_off_is_refused_and_untouched() {
    let rules = format!(
        "[[rule]]\ntool_result = \"missing_config\"\ntext = \"quiet is off\"\n{}",
        ask("ask quiet", "quiet", "hello?")
    );
    let home = bots::home("[[rule]]\nrepeat = true\ntext = \"hi\"\n");
    bot(home.path(), "alice", &rules, &[]);
    bot(home.path(), "quiet", "", &[("bot_mode.enabled", "false")]);
    let out = run(
        home.path(),
        &[
            "-p",
            "alice",
            "--json",
            "chat",
            "--bot-chat",
            "-z",
            "ask quiet",
        ],
    );
    let report: serde_json::Value = serde_json::from_str(&ok(&out)).unwrap();
    assert_eq!(report["answer"], "quiet is off", "{report}");
    let quiet = store(home.path(), "quiet");
    let all = pmt_store::ListOptions {
        all: true,
        ..pmt_store::ListOptions::default()
    };
    assert!(quiet.list_sessions(&all).unwrap().is_empty());
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let roster = runtime.block_on(async {
        let c = client(home.path(), &["default", "alice", "quiet"]).await;
        call(
            &c,
            "bot.roster.list",
            json!({"profile": "alice", "include_hidden": true}),
        )
        .await
    });
    let names: Vec<&str> = roster["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["agent"]["name"].as_str().unwrap())
        .collect();
    assert!(
        !names.contains(&"quiet") && names.contains(&"alice"),
        "{roster}"
    );
}

/// Scenarios: Headless Bot Chat; Forged call outside a bot chat; the
/// chain of bot messages is cut at the hop limit.
#[test]
fn only_a_bot_chat_has_message_agent() {
    let home = bots::home(
        "[[rule]]\ntools_regex = \"message_agent\"\ntext = \"I can message teammates\"\n\
         [[rule]]\ntool_result = \"unknown tool: message_agent\"\ntext = \"refused\"\n\
         [[rule]]\nuser = \"forge\"\n[[rule.tool_calls]]\nname = \"message_agent\"\n\
         arguments = { target = \"helper\", message = \"hi\" }\n",
    );
    bot(home.path(), "helper", "", &[]);
    let json_of = |out| -> serde_json::Value { serde_json::from_str(&ok(&out)).unwrap() };
    let headless = json_of(run(
        home.path(),
        &["--json", "chat", "--bot-chat", "-z", "who can you reach?"],
    ));
    assert_eq!(headless["answer"], "I can message teammates", "{headless}");
    let id = headless["session_id"].as_str().unwrap();
    assert_eq!(bot_chat(home.path(), "default").as_deref(), Some(id));
    let forged = json_of(run(home.path(), &["--json", "-z", "forge"]));
    assert_eq!(forged["answer"], "refused", "{forged}");
    assert!(
        store(home.path(), "helper")
            .bot_messages_in(Direction::In, "queued")
            .unwrap()
            .is_empty()
    );
}

/// The chain of bot-originated hops is cut at `bot_mode.max_hops`.
#[test]
fn a_chain_of_bot_messages_stops_at_the_hop_limit() {
    // Each bot sends once per turn a person or a teammate started; the
    // replies are only noted.
    let pinger = |other: &str| {
        format!(
            "[[rule]]\nuser_regex = \"^(Reply from|Your message)\"\nrepeat = true\ntext = \"noted\"\n\
             [[rule]]\ntool_result = \"hop limit\"\nrepeat = true\ntext = \"limit hit\"\n\
             {SENT}{}{}",
            ask("start", other, "ping"),
            ask("Message from", other, "ping").repeat(2),
        )
    };
    let home = bots::home("[[rule]]\nrepeat = true\ntext = \"hi\"\n");
    bot(
        home.path(),
        "alice",
        &pinger("bob"),
        &[("bot_mode.max_hops", "2")],
    );
    bot(
        home.path(),
        "bob",
        &pinger("alice"),
        &[("bot_mode.max_hops", "2")],
    );
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let c = client(home.path(), &["default", "alice", "bob"]).await;
        tell(&c, "alice", "start").await;
    });
    let answers = wait_said(home.path(), "alice", "assistant", "limit hit");
    assert!(answers.iter().any(|a| a == "limit hit"), "{answers:?}");
    let chat = bot_chat(home.path(), "alice").unwrap();
    let results = said(home.path(), "alice", &chat, "tool");
    assert!(
        results
            .iter()
            .any(|r| r.contains("agent message hop limit (2) reached [reason: hop_limit]")),
        "{results:?}"
    );
}

/// Scenario: Mention is resolved, not forwarded.
#[test]
fn a_mention_is_named_to_the_agent_and_never_forwarded() {
    let home = bots::home(
        "[[rule]]\nuser = \"[mention] @researcher is the bot profile `researcher`\"\ntext = \"noted the mention\"\n\
         [[rule]]\nrepeat = true\ntext = \"no mention seen\"\n",
    );
    bot(home.path(), "researcher", "", &[]);
    let out = run(
        home.path(),
        &[
            "--json",
            "chat",
            "--bot-chat",
            "-z",
            "@researcher have a look at this, or mail a@b.com",
        ],
    );
    let report: serde_json::Value = serde_json::from_str(&ok(&out)).unwrap();
    assert_eq!(report["answer"], "noted the mention", "{report}");
    let researcher = store(home.path(), "researcher");
    assert!(
        researcher
            .bot_messages_in(Direction::In, "queued")
            .unwrap()
            .is_empty()
    );
    assert!(bot_chat(home.path(), "researcher").is_none());
}

/// Scenario: New teammate mid-session.
#[test]
fn a_new_teammate_arrives_as_a_notice_and_the_prompt_stays() {
    let home = bots::home(
        "[[rule]]\nuser = \"[roster.update] added @carol\"\ntext = \"welcome carol\"\n\
         [[rule]]\nrepeat = true\ntext = \"ok\"\n",
    );
    bot(home.path(), "alice", "", &[]);
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let c = runtime.block_on(client(home.path(), &["default", "alice"]));
    let id = runtime.block_on(tell(&c, "default", "hello"));
    wait_said(home.path(), "default", "assistant", "ok");
    let hash = |home: &Path| {
        store(home, "default")
            .session(id.parse().unwrap())
            .unwrap()
            .system_prompt_hash
    };
    let before = hash(home.path());
    assert!(before.is_some());
    bot(home.path(), "carol", "", &[]);
    runtime.block_on(async {
        call(
            &c,
            "prompt.submit",
            json!({"session_id": id, "text": "again", "files": []}),
        )
        .await;
    });
    wait_said(home.path(), "default", "assistant", "welcome carol");
    assert_eq!(hash(home.path()), before, "the system prompt is unchanged");
}
