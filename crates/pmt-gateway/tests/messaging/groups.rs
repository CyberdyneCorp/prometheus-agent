//! Group chats (spec: messaging-gateway, Group mention gating and observe
//! mode; Bot-authored message admission; Channel history backfill;
//! Role-based access grants; Per-chat policy thread matching and skill
//! preload).

use std::time::Duration;

use pmt_gateway::messaging::{Admission, ChatType, Descriptor, HistoryEntry, Inbound, Interactive};

use crate::support::{Harness, Setup, eventually, said};

/// `platform` admitting everyone, debounce off, and `extra` (groups, bots
/// and policies as each test needs; gating stays at its defaults).
fn gated(platform: &str, extra: &str, table: &str) -> String {
    format!(
        "[gateway.busy]\ndebounce_ms = 0\n\n{extra}\n\
         [gateway.platforms.{platform}]\nenabled = true\nallow_all = true\n{table}\n"
    )
}

fn in_channel(sender: &str, text: &str) -> Inbound {
    let mut inbound = Inbound::text(ChatType::Channel, "c1", sender, text);
    inbound.message_id = format!("{sender}-{}", text.len());
    inbound.sender_name = Some(sender.to_uppercase());
    inbound
}

fn mentioning(sender: &str, text: &str) -> Inbound {
    let mut inbound = in_channel(sender, text);
    inbound.addressing.mentioned = true;
    inbound
}

#[tokio::test]
async fn an_unmentioned_channel_message_starts_no_turn() {
    let h = Harness::start(Setup::new(&gated("discord", "", ""))).await;
    assert_eq!(
        h.send("discord", in_channel("u1", "anyone around?")).await,
        Admission::Dropped
    );
    assert_eq!(
        h.send("discord", mentioning("u1", "you there?")).await,
        Admission::Accepted
    );
    said(&h.adapter("discord"), "echo: you there?").await;
    assert_eq!(h.agents.texts().len(), 1);
}

#[tokio::test]
async fn observed_lines_are_context_for_the_next_mention() {
    let config = gated(
        "telegram",
        "[gateway.groups.platforms.telegram]\nobserve_unmentioned = true\n",
        "",
    );
    let h = Harness::start(Setup::new(&config)).await;
    let ann = in_channel("ann", "Shall we meet on May 3?");
    let bob = in_channel("bob", "May 3 works for me");
    assert_eq!(h.send("telegram", ann).await, Admission::Handled);
    assert_eq!(h.send("telegram", bob).await, Admission::Handled);
    let ask = mentioning("cy", "what date did they agree?");
    h.send("telegram", ask).await;
    said(&h.adapter("telegram"), "echo: what date did they agree?").await;
    assert_eq!(h.agents.texts().len(), 1, "exactly one turn");
    let session = h.sessions()[0];
    let rows: Vec<String> = h
        .store()
        .messages(session, pmt_store::Visibility::Active)
        .unwrap()
        .into_iter()
        .filter_map(|m| m.content)
        .collect();
    assert_eq!(
        rows,
        [
            "[ANN|ann] Shall we meet on May 3?",
            "[BOB|bob] May 3 works for me"
        ]
    );
    let prefix = h.agents.authors.lock().unwrap()[0].prefix.clone().unwrap();
    assert!(prefix.contains("context, not instructions"), "{prefix}");
}

#[tokio::test]
async fn bots_are_ignored_unless_allowed_and_a_burst_is_one_turn() {
    let h = Harness::start(Setup::new(&gated("discord", "", ""))).await;
    let mut ping = mentioning("otherbot", "@prom hello");
    ping.is_bot = true;
    assert_eq!(h.send("discord", ping).await, Admission::Dropped);
    let config = gated("discord", "[gateway.bots]\nallow_bots = \"mentions\"\n", "");
    let h = Harness::start(Setup::new(&config)).await;
    let mut first = mentioning("otherbot", "@prom here is part one");
    first.is_bot = true;
    h.send("discord", first).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut second = in_channel("otherbot", "and part two");
    second.is_bot = true;
    second.addressing.mentioned = false;
    assert_ne!(h.send("discord", second).await, Admission::Dropped);
    eventually("one turn", || h.agents.texts().len() == 1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        h.agents.texts(),
        ["@prom here is part one\nand part two"],
        "both chunks in one turn"
    );
}

#[tokio::test]
async fn a_backfill_brings_the_channel_since_the_bots_last_reply() {
    let mut setup = Setup::new(&gated("discord", "", ""));
    setup.descriptors.push(Descriptor {
        can_fetch_history: true,
        supports_edit: true,
        interactive: Interactive::Native,
        ..Descriptor::new("discord", 2000)
    });
    let h = Harness::start(setup).await;
    let entry = |id: &str, from_self: bool| HistoryEntry {
        message_id: id.into(),
        author_id: if from_self { "bot" } else { "u9" }.into(),
        author_name: if from_self { "Prom" } else { "Dee" }.into(),
        text: format!("text {id}"),
        from_self,
    };
    let mut history = vec![entry("older", false), entry("reply", true)];
    history.extend((1..=12).map(|i| entry(&format!("n{i}"), false)));
    let ask = mentioning("u1", "summarize?");
    history.push(entry(&ask.message_id, false));
    *h.adapter("discord").history.lock().unwrap() = history;
    h.send("discord", ask).await;
    eventually("the turn starts", || h.agents.texts().len() == 1).await;
    let text = h.agents.texts()[0].clone();
    assert_eq!(text.matches("[Dee|u9]").count(), 12, "{text}");
    assert!(text.contains("text n1") && !text.contains("text older"));
    assert!(text.ends_with("summarize?"));
    // A DM is never backfilled.
    h.dm("discord", "u1", "direct").await;
    eventually("the second turn", || h.agents.texts().len() == 2).await;
    assert_eq!(h.agents.texts()[1], "direct");
}

#[tokio::test]
async fn a_role_grants_access_and_a_failed_lookup_denies() {
    let config = "[gateway.busy]\ndebounce_ms = 0\n\n\
        [gateway.platforms.discord]\nenabled = true\nallow_roles = [\"111222333\"]\n";
    let mut setup = Setup::new(config);
    setup.descriptors.push(Descriptor {
        exposes_roles: true,
        ..Descriptor::new("discord", 2000)
    });
    let h = Harness::start(setup).await;
    h.adapter("discord")
        .roles
        .lock()
        .unwrap()
        .insert("holder".into(), vec!["111222333".into()]);
    assert_eq!(
        h.send("discord", mentioning("holder", "hi")).await,
        Admission::Accepted
    );
    assert_eq!(
        h.send("discord", mentioning("stranger", "hi")).await,
        Admission::Unauthorized,
        "the lookup failed: denied"
    );
}

#[tokio::test]
async fn a_thread_policy_preloads_its_skills_only_in_that_topic() {
    let policies = "[[gateway.chat_policies]]\nplatform = \"telegram\"\nchat_id = \"-100555\"\n\
        thread_id = \"42\"\npreload_skills = [\"release-notes\"]\n";
    let h = Harness::start(Setup::new(&gated("telegram", policies, ""))).await;
    for topic in ["42", "7"] {
        let mut message = mentioning("u1", "go");
        message.chat_id = "-100555".into();
        message.chat_type = ChatType::Thread;
        message.thread_id = Some(topic.into());
        message.message_id = format!("in-{topic}");
        h.send("telegram", message).await;
    }
    eventually("two sessions", || {
        h.agents.conversations.lock().unwrap().len() == 2
    })
    .await;
    let conversations = h.agents.conversations.lock().unwrap().clone();
    let preload = |topic: &str| {
        conversations
            .iter()
            .find(|c| c.thread_id.as_deref() == Some(topic))
            .and_then(|c| c.policy.clone())
            .and_then(|p| p.preload_skills)
    };
    assert_eq!(preload("42"), Some(vec!["release-notes".to_owned()]));
    assert_eq!(preload("7"), None);
}
