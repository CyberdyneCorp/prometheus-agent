//! Adapter features the host drives (spec: platform-adapters, Native
//! command menus and slash registration; Telegram topics and per-topic
//! skills; Webhook routes): command menus registered at connect and after
//! a reload, and session hints shaping the sessions of one thread.

use pmt_gateway::messaging::{ChatType, Inbound, MenuCommand, SessionHints};

use crate::support::{Harness, Setup, eventually, open, said};

#[tokio::test]
async fn the_command_menu_is_registered_at_connect_and_after_a_reload() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    let adapter = h.adapter("telegram");
    eventually("the menu is registered at connect", || {
        !adapter.menus.lock().unwrap().is_empty()
    })
    .await;
    *h.agents.menu.lock().unwrap() = vec![MenuCommand {
        name: "arxiv".into(),
        description: "search papers".into(),
        takes_args: true,
    }];
    h.gateway.reload();
    eventually("the menu is registered again", || {
        adapter
            .menus
            .lock()
            .unwrap()
            .last()
            .is_some_and(|m| m.iter().any(|c| c.name == "arxiv"))
    })
    .await;
}

#[tokio::test]
async fn a_topic_skill_preloads_into_that_topics_own_session() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.adapter("telegram").hints.lock().unwrap().insert(
        "7".into(),
        SessionHints {
            skills: vec!["arxiv".into()],
            toolsets: None,
        },
    );
    h.dm("telegram", "42", "root hello").await;
    said(&h.adapter("telegram"), "echo: root hello").await;
    let mut topic = Inbound::text(ChatType::Dm, "42", "42", "find papers");
    topic.thread_id = Some("7".into());
    h.send("telegram", topic).await;
    said(&h.adapter("telegram"), "echo: find papers").await;
    let conversations = h.agents.conversations.lock().unwrap().clone();
    assert_eq!(conversations.len(), 2, "the topic has its own session");
    let root = conversations
        .iter()
        .find(|c| c.thread_id.is_none())
        .unwrap();
    let research = conversations
        .iter()
        .find(|c| c.thread_id.as_deref() == Some("7"))
        .unwrap();
    assert!(
        root.policy
            .as_ref()
            .and_then(|p| p.skills.clone())
            .is_none(),
        "{:?}",
        root.policy
    );
    assert_eq!(
        research.policy.as_ref().and_then(|p| p.skills.clone()),
        Some(vec!["arxiv".to_owned()])
    );
    assert_eq!(h.sessions().len(), 2);
}
