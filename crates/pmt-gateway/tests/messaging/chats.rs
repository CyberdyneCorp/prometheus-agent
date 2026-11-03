//! A chat's model and sessions (spec: messaging-gateway, Interactive model
//! picker and chat model override).

use pmt_gateway::messaging::{ChatType, Descriptor, Inbound, Interactive, ModelChoice};

use crate::support::{Did, Harness, Setup, eventually, open, said};

fn choices() -> Vec<ModelChoice> {
    let many = (0..10).map(|i| format!("model-{i}")).collect();
    vec![
        ModelChoice {
            provider: "anthropic".into(),
            models: vec!["claude-sonnet-4.5".into(), "claude-haiku-4.5".into()],
        },
        ModelChoice {
            provider: "openrouter".into(),
            models: many,
        },
    ]
}

/// The model the newest conversation session was created with.
fn last_model(h: &Harness) -> Option<String> {
    h.agents
        .conversations
        .lock()
        .unwrap()
        .last()
        .and_then(|c| c.policy.as_ref())
        .and_then(|p| p.model.clone())
}

fn press(data: &str) -> Inbound {
    let mut press = Inbound::text(ChatType::Dm, "1", "1", "");
    press.callback = Some(data.to_owned());
    press
}

/// The latest card the adapter sent or edited.
fn last_card(h: &Harness) -> pmt_gateway::messaging::Outgoing {
    match h.adapter("telegram").all().last().cloned() {
        Some(Did::Send(m) | Did::Edit(_, m)) => m,
        None => panic!("nothing sent"),
    }
}

/// Spec scenario: Override survives /new.
#[tokio::test]
async fn a_picked_model_survives_new() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    *h.agents.models.lock().unwrap() = choices();
    h.dm("telegram", "1", "/model").await;
    said(&h.adapter("telegram"), "Choose a provider:").await;
    let providers = last_card(&h);
    assert_eq!(providers.buttons[0].label, "anthropic");
    h.send("telegram", press(&providers.buttons[0].data)).await;
    eventually("the provider's models are shown", || {
        last_card(&h).text == "Choose a anthropic model:"
    })
    .await;
    let models = last_card(&h);
    assert_eq!(models.buttons[0].label, "claude-sonnet-4.5");
    h.send("telegram", press(&models.buttons[0].data)).await;
    said(&h.adapter("telegram"), "model switched").await;
    let commands = h.agents.commands.lock().unwrap().clone();
    assert!(
        commands
            .iter()
            .any(|(_, line)| line == "/model anthropic/claude-sonnet-4.5"),
        "{commands:?}"
    );
    h.dm("telegram", "1", "/new").await;
    said(&h.adapter("telegram"), "New session started.").await;
    assert_eq!(
        last_model(&h).as_deref(),
        Some("anthropic/claude-sonnet-4.5")
    );
    h.dm("telegram", "1", "/model status").await;
    said(
        &h.adapter("telegram"),
        "Chat override: anthropic/claude-sonnet-4.5",
    )
    .await;
    h.dm("telegram", "1", "/model --clear").await;
    said(&h.adapter("telegram"), "override cleared").await;
    h.dm("telegram", "1", "/new").await;
    eventually("a third session", || {
        h.agents.conversations.lock().unwrap().len() == 3
    })
    .await;
    assert_eq!(last_model(&h), None, "cleared");
}

#[tokio::test]
async fn models_page_eight_at_a_time() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    *h.agents.models.lock().unwrap() = choices();
    h.dm("telegram", "1", "/model").await;
    said(&h.adapter("telegram"), "Choose a provider:").await;
    let providers = last_card(&h);
    h.send("telegram", press(&providers.buttons[1].data)).await;
    eventually("openrouter's first page", || {
        last_card(&h).text == "Choose a openrouter model:"
    })
    .await;
    let first = last_card(&h);
    let labels: Vec<&str> = first.buttons.iter().map(|b| b.label.as_str()).collect();
    assert_eq!(labels.len(), 10, "8 models, back, next: {labels:?}");
    assert_eq!(labels[7], "model-7");
    let next = first.buttons.last().unwrap();
    assert_eq!(next.label, "Next \u{203a}");
    h.send("telegram", press(&next.data)).await;
    eventually("the second page", || {
        last_card(&h).buttons.iter().any(|b| b.label == "model-9")
    })
    .await;
}

#[tokio::test]
async fn without_buttons_the_models_are_numbered() {
    let mut setup = Setup::new(&open("telegram"));
    setup.descriptors.push(Descriptor {
        interactive: Interactive::Numbered,
        ..Descriptor::new("telegram", 4096)
    });
    let h = Harness::start(setup).await;
    *h.agents.models.lock().unwrap() = choices();
    h.dm("telegram", "1", "/model").await;
    said(&h.adapter("telegram"), "2. anthropic/claude-haiku-4.5").await;
    h.dm("telegram", "1", "/model 2").await;
    said(&h.adapter("telegram"), "model switched").await;
    h.dm("telegram", "1", "/new").await;
    said(&h.adapter("telegram"), "New session started.").await;
    assert_eq!(
        last_model(&h).as_deref(),
        Some("anthropic/claude-haiku-4.5")
    );
}

#[tokio::test]
async fn resume_switches_back_to_an_earlier_session_of_the_chat() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.dm("telegram", "1", "first").await;
    said(&h.adapter("telegram"), "echo: first").await;
    h.dm("telegram", "1", "/new").await;
    said(&h.adapter("telegram"), "New session started.").await;
    h.dm("telegram", "1", "second").await;
    said(&h.adapter("telegram"), "echo: second").await;
    h.dm("telegram", "1", "/sessions").await;
    said(&h.adapter("telegram"), "1. untitled").await;
    let listing = h.adapter("telegram").texts().last().cloned().unwrap();
    assert!(listing.contains("2. untitled"), "{listing}");
    assert!(listing.contains("(current)"), "{listing}");
    h.dm("telegram", "1", "/resume 2").await;
    said(&h.adapter("telegram"), "Resumed").await;
    h.dm("telegram", "1", "again").await;
    said(&h.adapter("telegram"), "echo: again").await;
    let sessions = h.agents.turn_sessions();
    assert_eq!(sessions[2], sessions[0], "the first session took the turn");
}

/// Spec scenario: Main chat stays responsive.
#[tokio::test]
async fn a_background_task_reports_back_while_the_chat_keeps_talking() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.dm("telegram", "1", "/bg slow:400:the report").await;
    said(
        &h.adapter("telegram"),
        "Background task started: slow:400:the report (id bg_",
    )
    .await;
    h.dm("telegram", "1", "hello").await;
    said(&h.adapter("telegram"), "echo: hello").await;
    let texts = h.adapter("telegram").texts();
    assert!(
        !texts.iter().any(|t| t.contains("Background task complete")),
        "the chat answered before the task ended: {texts:?}"
    );
    said(
        &h.adapter("telegram"),
        "✅ Background task complete\n\nthe report",
    )
    .await;
    let sessions = h.agents.turn_sessions();
    assert_eq!(sessions.len(), 2);
    assert_ne!(
        sessions[0], sessions[1],
        "the task ran in a session of its own"
    );
    let conversations = h.agents.conversations.lock().unwrap().clone();
    assert!(
        conversations.iter().all(|c| c.chat_id == "1"),
        "the chat is its origin"
    );
}

#[tokio::test]
async fn a_chat_runs_at_most_three_background_tasks_and_failures_say_why() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    // Long enough to outlive the paced delivery of the notices.
    for _ in 0..3 {
        h.dm("telegram", "1", "/background slow:30000").await;
    }
    eventually("three tasks started", || {
        h.adapter("telegram")
            .texts()
            .iter()
            .filter(|t| t.starts_with("Background task started"))
            .count()
            == 3
    })
    .await;
    h.dm("telegram", "1", "/bg one more").await;
    said(
        &h.adapter("telegram"),
        "Too many background tasks running (3)",
    )
    .await;
    h.dm("telegram", "2", "/bg fail").await;
    let other = h.adapter("telegram");
    said(&other, "❌ Background task failed: the provider failed").await;
}

fn threaded() -> Descriptor {
    Descriptor {
        supports_edit: true,
        supports_threads: true,
        interactive: Interactive::Native,
        ..Descriptor::new("slack", 4096)
    }
}

/// Spec scenario: Branch opens a thread.
#[tokio::test]
async fn a_branch_opens_a_sibling_thread_bound_to_the_fork() {
    let mut setup = Setup::new(&open("slack"));
    setup.descriptors.push(threaded());
    let h = Harness::start(setup).await;
    let channel = |text: &str| Inbound::text(ChatType::Group, "C1", "U1", text);
    h.send("slack", channel("hello")).await;
    said(&h.adapter("slack"), "echo: hello").await;
    h.send("slack", channel("/branch try-postgres")).await;
    said(
        &h.adapter("slack"),
        "Branch \"try-postgres\" opened in a new thread",
    )
    .await;
    let threads = h.adapter("slack").threads.lock().unwrap().clone();
    assert_eq!(
        threads,
        [(
            "C1".to_owned(),
            "try-postgres".to_owned(),
            "th-1".to_owned()
        )]
    );
    let mut in_thread = channel("in the branch");
    in_thread.thread_id = Some("th-1".into());
    h.send("slack", in_thread).await;
    said(&h.adapter("slack"), "echo: in the branch").await;
    h.send("slack", channel("still here")).await;
    said(&h.adapter("slack"), "echo: still here").await;
    let sessions = h.agents.turn_sessions();
    assert_eq!(
        sessions[0], sessions[2],
        "the channel kept the original session"
    );
    assert_ne!(sessions[1], sessions[0], "the thread runs the fork");
    let fork = h.store().session(sessions[1].parse().unwrap()).unwrap();
    assert_eq!(fork.lineage_kind, Some(pmt_store::LineageKind::Fork));
    assert_eq!(
        fork.parent_session_id.map(|p| p.to_string()),
        Some(sessions[0].clone())
    );
}

#[tokio::test]
async fn a_branch_here_switches_the_chat_and_a_running_turn_refuses() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.dm("telegram", "1", "slow:300:the answer").await;
    h.dm("telegram", "1", "/branch").await;
    said(
        &h.adapter("telegram"),
        "/branch is unavailable while the agent is working; use /stop first",
    )
    .await;
    said(&h.adapter("telegram"), "the answer").await;
    h.dm("telegram", "1", "/fork --here").await;
    said(&h.adapter("telegram"), "Switched to the branch").await;
    h.dm("telegram", "1", "next").await;
    said(&h.adapter("telegram"), "echo: next").await;
    let sessions = h.agents.turn_sessions();
    assert_ne!(sessions[0], sessions[1], "the chat now runs the fork");
}

fn topics(threads: bool) -> Descriptor {
    Descriptor {
        supports_edit: true,
        supports_threads: threads,
        dm_topics: true,
        interactive: Interactive::Native,
        ..Descriptor::new("telegram", 4096)
    }
}

fn in_topic(thread: &str, text: &str) -> Inbound {
    let mut inbound = Inbound::text(ChatType::Dm, "1", "1", text);
    inbound.thread_id = Some(thread.to_owned());
    inbound
}

/// Spec scenario: Lobby redirects.
#[tokio::test]
async fn the_root_dm_is_a_lobby_and_each_topic_is_a_session() {
    let mut setup = Setup::new(&open("telegram"));
    setup.descriptors.push(topics(true));
    let h = Harness::start(setup).await;
    let admission = h.dm("telegram", "1", "hello").await;
    assert_eq!(admission, pmt_gateway::messaging::Admission::Handled);
    said(
        &h.adapter("telegram"),
        "Open or create a topic to chat (/topic new <name>)",
    )
    .await;
    assert!(h.agents.texts().is_empty(), "the lobby starts no turn");
    h.dm("telegram", "1", "/topic new Research").await;
    said(&h.adapter("telegram"), "Topic \"Research\" created.").await;
    h.dm("telegram", "1", "/topic new").await;
    said(&h.adapter("telegram"), "Topic \"New chat\" created.").await;
    h.send("telegram", in_topic("th-1", "first topic")).await;
    said(&h.adapter("telegram"), "echo: first topic").await;
    h.send("telegram", in_topic("th-2", "second topic")).await;
    said(&h.adapter("telegram"), "echo: second topic").await;
    h.send("telegram", in_topic("th-1", "/new")).await;
    said(&h.adapter("telegram"), "New session started.").await;
    h.send("telegram", in_topic("th-1", "fresh")).await;
    said(&h.adapter("telegram"), "echo: fresh").await;
    let sessions = h.agents.turn_sessions();
    assert_ne!(sessions[0], sessions[1], "each topic has its session");
    assert_ne!(sessions[0], sessions[2], "/new in a topic starts another");
    h.dm("telegram", "1", "/topic list").await;
    said(&h.adapter("telegram"), "- topic th-1:").await;
}

/// Spec scenario: Restore into a new topic.
#[tokio::test]
async fn a_restored_topic_continues_the_earlier_session() {
    let mut setup = Setup::new(&open("telegram"));
    setup.descriptors.push(topics(true));
    let h = Harness::start(setup).await;
    h.dm("telegram", "1", "/topic new Plans").await;
    said(&h.adapter("telegram"), "Topic \"Plans\" created.").await;
    h.send("telegram", in_topic("th-1", "remember")).await;
    said(&h.adapter("telegram"), "echo: remember").await;
    let earlier = h.agents.turn_sessions()[0].clone();
    h.store()
        .set_title(
            earlier.parse().unwrap(),
            Some("Trip plans"),
            pmt_store::TitleSource::User,
        )
        .unwrap();
    h.dm("telegram", "1", &format!("/topic restore {earlier}"))
        .await;
    said(
        &h.adapter("telegram"),
        "Topic \"Trip plans\" continues session",
    )
    .await;
    let threads = h.adapter("telegram").threads.lock().unwrap().clone();
    assert_eq!(threads[1].1, "Trip plans");
    h.send("telegram", in_topic("th-2", "where were we")).await;
    said(&h.adapter("telegram"), "echo: where were we").await;
    assert_eq!(h.agents.turn_sessions()[1], earlier);
}

#[tokio::test]
async fn a_chat_without_topics_falls_back_to_one_session() {
    let mut setup = Setup::new(&open("telegram"));
    setup.descriptors.push(topics(false));
    let h = Harness::start(setup).await;
    h.dm("telegram", "1", "/topic new Research").await;
    said(
        &h.adapter("telegram"),
        "Topics are unavailable in this chat",
    )
    .await;
    h.dm("telegram", "1", "hello").await;
    said(&h.adapter("telegram"), "echo: hello").await;
}

#[tokio::test]
async fn a_default_named_topic_takes_the_sessions_title() {
    let mut setup = Setup::new(&open("telegram"));
    setup.descriptors.push(topics(true));
    let h = Harness::start(setup).await;
    h.dm("telegram", "1", "/topic new").await;
    said(&h.adapter("telegram"), "Topic \"New chat\" created.").await;
    h.send("telegram", in_topic("th-1", "slow:300:go")).await;
    eventually("the topic's first turn runs", || {
        !h.agents.turn_sessions().is_empty()
    })
    .await;
    // The title the session gets during its first turn.
    let session = h.agents.turn_sessions()[0].clone();
    h.store()
        .set_title(
            session.parse().unwrap(),
            Some("Budget review"),
            pmt_store::TitleSource::User,
        )
        .unwrap();
    said(&h.adapter("telegram"), "go").await;
    eventually("the topic is renamed", || {
        h.adapter("telegram").renamed.lock().unwrap().clone()
            == [("th-1".to_owned(), "Budget review".to_owned())]
    })
    .await;
}
