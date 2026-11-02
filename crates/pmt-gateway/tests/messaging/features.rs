//! Adapter features the host drives (spec: platform-adapters, Native
//! command menus and slash registration; Telegram topics and per-topic
//! skills; Webhook routes): command menus registered at connect and after
//! a reload, and session hints shaping the sessions of one thread.

use pmt_gateway::messaging::{
    ChatType, Descriptor, Inbound, Markup, MediaKind, MenuCommand, Outgoing, SendError,
    SessionHints,
};

use crate::support::{Did, Harness, Setup, eventually, open, said};

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

fn telegram(markup: Markup) -> Setup {
    let mut setup = Setup::new(&open("telegram"));
    setup.descriptors.push(Descriptor {
        markup,
        ..Descriptor::new("telegram", 4096)
    });
    setup
}

fn sends(adapter: &crate::support::FakeAdapter) -> Vec<Outgoing> {
    adapter
        .all()
        .into_iter()
        .filter_map(|d| match d {
            Did::Send(m) => Some(m),
            Did::Edit(..) => None,
        })
        .collect()
}

#[tokio::test]
async fn replies_are_rendered_in_the_platform_markup_and_resent_plain_when_refused() {
    let h = Harness::start(telegram(Markup::Platform)).await;
    h.dm("telegram", "1", "say:**Done** see [docs](https://x.y).")
        .await;
    said(&h.adapter("telegram"), "*Done* see [docs](https://x.y)\\.").await;

    h.adapter("telegram")
        .failures
        .lock()
        .unwrap()
        .push_back(SendError::Formatting("can't parse entities".into()));
    h.dm("telegram", "1", "say:**Again** [docs](https://x.y)")
        .await;
    said(&h.adapter("telegram"), "**Again** docs (https://x.y)").await;
    let last = sends(&h.adapter("telegram")).pop().unwrap();
    assert!(last.plain);
}

#[tokio::test]
async fn a_refused_rich_message_falls_back_to_markdown_v2_before_plain_text() {
    let h = Harness::start(telegram(Markup::Html)).await;
    h.adapter("telegram")
        .failures
        .lock()
        .unwrap()
        .push_back(SendError::Formatting("rich message refused".into()));
    h.dm("telegram", "1", "say:**Done**").await;
    said(&h.adapter("telegram"), "*Done*").await;
    let tries = sends(&h.adapter("telegram"));
    let fallback = tries.last().unwrap();
    assert_eq!(fallback.markup, Some(Markup::Platform));
    assert!(!fallback.plain);
}

fn reacting(platform: &str, extra: &str) -> Setup {
    let mut setup = Setup::new(&format!("{}{extra}", open(platform)));
    setup.descriptors.push(Descriptor {
        supports_reactions: true,
        ..Descriptor::new(platform, 2000)
    });
    setup
}

fn emojis(adapter: &crate::support::FakeAdapter) -> Vec<(String, String)> {
    adapter
        .reactions
        .lock()
        .unwrap()
        .iter()
        .map(|r| (r.message_id.clone(), r.emoji.clone()))
        .collect()
}

fn from(sender: &str, id: &str, text: &str) -> Inbound {
    let mut inbound = Inbound::text(ChatType::Dm, sender, sender, text);
    inbound.message_id = id.into();
    inbound
}

#[tokio::test]
async fn a_failed_turn_is_marked_with_eyes_and_a_cross_on_discord() {
    let h = Harness::start(reacting("discord", "")).await;
    h.send("discord", from("7", "m1", "fail")).await;
    eventually("both reactions", || {
        emojis(&h.adapter("discord")).len() == 2
    })
    .await;
    assert_eq!(
        emojis(&h.adapter("discord")),
        vec![
            ("m1".to_owned(), "\u{1f440}".to_owned()),
            ("m1".to_owned(), "\u{274c}".to_owned())
        ]
    );
    h.send("discord", from("7", "m2", "hello")).await;
    said(&h.adapter("discord"), "echo: hello").await;
    eventually("the check mark", || {
        emojis(&h.adapter("discord")).last() == Some(&("m2".to_owned(), "\u{2705}".to_owned()))
    })
    .await;
}

#[tokio::test]
async fn telegram_marks_nothing_unless_reactions_are_turned_on() {
    let h = Harness::start(reacting("telegram", "")).await;
    h.send("telegram", from("7", "m1", "hello")).await;
    said(&h.adapter("telegram"), "echo: hello").await;
    assert!(emojis(&h.adapter("telegram")).is_empty());

    let h = Harness::start(reacting("telegram", "reactions = true\n")).await;
    h.send("telegram", from("7", "m1", "hello")).await;
    said(&h.adapter("telegram"), "echo: hello").await;
    eventually("eyes then a check mark", || {
        emojis(&h.adapter("telegram")).len() == 2
    })
    .await;
}

fn tier(platform: &str, interactive: pmt_gateway::messaging::Interactive) -> Setup {
    let mut setup = Setup::new(&open(platform));
    setup.descriptors.push(Descriptor {
        interactive,
        ..Descriptor::new(platform, 4000)
    });
    setup
}

#[tokio::test]
async fn a_numbered_multi_select_takes_several_numbers_and_refuses_out_of_range() {
    let h = Harness::start(tier(
        "signal",
        pmt_gateway::messaging::Interactive::Numbered,
    ))
    .await;
    let adapter = h.adapter("signal");
    h.dm("signal", "1", "clarify-multi").await;
    said(&adapter, "Multiple selections allowed").await;
    h.dm("signal", "1", "7").await;
    said(&adapter, "Valid choices are 1-4.").await;
    h.dm("signal", "1", "1, 3").await;
    said(
        &adapter,
        r#"clarified: [{"choices":["mon","wed"],"status":"answered"}]"#,
    )
    .await;
}

#[tokio::test]
async fn a_native_multi_select_control_is_submitted_with_its_picks() {
    let h = Harness::start(tier("discord", pmt_gateway::messaging::Interactive::Native)).await;
    let adapter = h.adapter("discord");
    h.dm("discord", "1", "clarify-multi").await;
    said(&adapter, "Multiple selections allowed").await;
    let card = sends(&adapter)
        .into_iter()
        .find(|m| m.select.is_some())
        .expect("a select control");
    let select = card.select.unwrap();
    assert_eq!(select.options, ["mon", "tue", "wed", "thu"]);
    assert!(card.buttons.is_empty());
    let mut press = Inbound::text(ChatType::Dm, "1", "1", "");
    press.callback = Some(pmt_gateway::messaging::Select::submission(
        &select.data,
        &[2, 4],
    ));
    h.send("discord", press).await;
    said(
        &adapter,
        r#"clarified: [{"choices":["tue","thu"],"status":"answered"}]"#,
    )
    .await;
}

/// A Telegram-like adapter sending every media kind, and a session
/// workspace holding `files`.
async fn with_workspace(files: &[&str]) -> (Harness, std::path::PathBuf) {
    let all = [
        MediaKind::Image,
        MediaKind::Audio,
        MediaKind::Voice,
        MediaKind::Video,
        MediaKind::Document,
    ];
    let mut setup = Setup::new(&open("telegram"));
    setup.descriptors.push(Descriptor {
        media_out: all.into_iter().collect(),
        ..Descriptor::new("telegram", 4096)
    });
    let h = Harness::start(setup).await;
    let workspace = h.home.path().join("workspace");
    std::fs::create_dir_all(workspace.join("out")).unwrap();
    for file in files {
        std::fs::write(workspace.join(file), *file).unwrap();
    }
    *h.agents.cwd.lock().unwrap() = Some(workspace.to_string_lossy().into_owned());
    (h, workspace)
}

fn last_send(h: &Harness) -> Outgoing {
    sends(&h.adapter("telegram")).pop().unwrap()
}

#[tokio::test]
async fn a_report_path_in_the_reply_becomes_a_document_upload() {
    let (h, workspace) = with_workspace(&["report.pdf"]).await;
    let path = workspace.join("report.pdf");
    h.dm("telegram", "1", &format!("say:Done: {}", path.display()))
        .await;
    said(&h.adapter("telegram"), "Done:").await;
    let sent = last_send(&h);
    assert_eq!(sent.text, "Done:");
    assert_eq!(sent.attachments.len(), 1);
    assert!(sent.attachments[0].as_document);
    assert!(sent.attachments[0].path.ends_with("report.pdf"));
}

#[tokio::test]
async fn a_path_inside_a_code_block_is_delivered_unchanged() {
    let (h, workspace) = with_workspace(&["a.png"]).await;
    let reply = format!("Look:\n```\n{}\n```", workspace.join("a.png").display());
    h.dm("telegram", "1", &format!("say:{reply}")).await;
    said(&h.adapter("telegram"), "Look:").await;
    let sent = last_send(&h);
    assert!(sent.attachments.is_empty());
    assert!(sent.text.contains("a.png"), "{}", sent.text);
}

#[tokio::test]
async fn as_document_makes_a_skill_photo_go_uncompressed() {
    let (h, workspace) = with_workspace(&["photo.jpg"]).await;
    let photo = workspace.join("photo.jpg");
    h.dm(
        "telegram",
        "1",
        &format!("say:Here\n[[as_document]] MEDIA:{}", photo.display()),
    )
    .await;
    said(&h.adapter("telegram"), "Here").await;
    let sent = last_send(&h);
    assert_eq!(sent.text, "Here");
    assert!(!sent.text.contains("[[as_document]]"));
    assert!(sent.attachments[0].as_document);
}

#[tokio::test]
async fn a_chart_rendered_in_the_container_is_delivered_from_the_host() {
    let (h, workspace) = with_workspace(&["out/chart.png"]).await;
    *h.agents.container.lock().unwrap() =
        Some(pmt_gateway::messaging::attachments::ContainerMounts {
            mounts: vec![("/workspace".into(), workspace.clone())],
            home: Some("/root/.prometheus".into()),
        });
    h.dm("telegram", "1", "say:Chart: /workspace/out/chart.png")
        .await;
    said(&h.adapter("telegram"), "Chart:").await;
    let sent = last_send(&h);
    assert_eq!(sent.attachments.len(), 1);
    assert!(!sent.attachments[0].as_document, "an image goes inline");
    assert_eq!(
        std::path::Path::new(&sent.attachments[0].path),
        std::fs::canonicalize(workspace.join("out/chart.png")).unwrap()
    );
}

#[tokio::test]
async fn a_completed_kanban_task_uploads_its_artifacts_to_a_slack_channel() {
    let mut setup = Setup::new(&format!(
        "{}[gateway.platforms.slack.home_channel]\nchat_id = \"C1\"\n",
        open("slack")
    ));
    setup.descriptors.push(Descriptor {
        media_out: [MediaKind::Image, MediaKind::Document]
            .into_iter()
            .collect(),
        ..Descriptor::new("slack", 39_000)
    });
    let h = Harness::start(setup).await;
    let report = h.home.path().join("report.xlsx");
    std::fs::write(&report, "xlsx").unwrap();
    let gone = h.home.path().join("gone.csv");
    h.gateway
        .deliver_artifacts("default", "slack", "Task done", &[report, gone])
        .await
        .unwrap();
    said(&h.adapter("slack"), "Task done").await;
    let sent = sends(&h.adapter("slack")).pop().unwrap();
    assert_eq!(sent.chat_id, "C1");
    assert_eq!(sent.attachments.len(), 1);
    assert!(sent.attachments[0].path.ends_with("report.xlsx"));
    assert!(sent.attachments[0].as_document);
    assert!(
        sent.text.contains("Skipped missing artifacts: gone.csv"),
        "{}",
        sent.text
    );
}

fn slack_thread(text: &str) -> Inbound {
    let mut inbound = Inbound::text(ChatType::Thread, "C1", "U1", text);
    inbound.thread_id = Some("1700.1".into());
    inbound
}

#[tokio::test]
async fn a_bang_stop_after_the_bot_mention_stops_the_thread_turn() {
    let mut setup = Setup::new(&open("slack"));
    setup.descriptors.push(Descriptor {
        command_prefix_alias: Some("!".into()),
        ..Descriptor::new("slack", 39_000)
    });
    let h = Harness::start(setup).await;
    h.send("slack", slack_thread("slow:10000")).await;
    eventually("the turn runs", || h.agents.running() == 1).await;
    h.send("slack", slack_thread("<@B1> !stop")).await;
    said(
        &h.adapter("slack"),
        "Stopped. You can continue this session.",
    )
    .await;
    eventually("the turn ends", || h.agents.running() == 0).await;
    h.send("slack", slack_thread("please run !stop later"))
        .await;
    said(&h.adapter("slack"), "echo: please run !stop later").await;
}
