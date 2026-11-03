//! How turns show in a chat (spec: messaging-gateway, Display policy and
//! per-platform overrides; Runtime metadata footer; Warning notification
//! suppression; Intentional silence tokens).

use std::time::Duration;

use pmt_gateway::messaging::{ChatType, Descriptor, Inbound, Interactive, SendRequest};
use pmt_store::gateway::outbox::OutboundState;

use crate::support::{Did, Harness, Setup, eventually, open, said};

fn both(extra: &str) -> String {
    format!(
        "[gateway.busy]\ndebounce_ms = 0\n\n[gateway.groups]\nrequire_mention = false\n\n{extra}\n\
         [gateway.platforms.telegram]\nenabled = true\nallow_all = true\n\
         home_channel = {{ chat_id = \"tg-home\" }}\n\n\
         [gateway.platforms.discord]\nenabled = true\nallow_all = true\n\n\
         [gateway.platforms.slack]\nenabled = true\nallow_all = true\n\
         home_channel = {{ chat_id = \"slack-home\" }}\n"
    )
}

fn deletable(platform: &str) -> Descriptor {
    Descriptor {
        supports_edit: true,
        supports_delete: true,
        interactive: Interactive::Native,
        ..Descriptor::new(platform, 4096)
    }
}

fn progress_sends(h: &Harness, platform: &str) -> Vec<String> {
    h.adapter(platform)
        .texts()
        .into_iter()
        .filter(|t| t.contains("\u{2699} terminal"))
        .collect()
}

#[tokio::test]
async fn by_default_telegram_shows_no_progress_and_discord_one_accumulating_bubble() {
    let h = Harness::start(Setup::new(&both(""))).await;
    h.dm("telegram", "1", "tools:3").await;
    said(&h.adapter("telegram"), "tools done").await;
    assert!(progress_sends(&h, "telegram").is_empty());
    h.dm("discord", "1", "tools:3").await;
    said(&h.adapter("discord"), "tools done").await;
    assert_eq!(progress_sends(&h, "discord").len(), 1, "one bubble");
    let edits: Vec<String> = h
        .adapter("discord")
        .all()
        .into_iter()
        .filter_map(|d| match d {
            Did::Edit(_, m) if m.text.contains("\u{2699} terminal") => Some(m.text),
            _ => None,
        })
        .collect();
    let last = edits.last().expect("the bubble was edited");
    assert_eq!(last.matches("\u{2699} terminal").count(), 3, "{last}");
    assert!(
        h.adapter("discord")
            .texts()
            .contains(&"Let me look.".to_owned()),
        "the interim message is shown"
    );
}

#[tokio::test]
async fn previews_are_cut_and_separate_grouping_sends_a_line_per_call() {
    let h = Harness::start(Setup::new(&both(
        "[gateway.display.platforms.discord]\ntool_progress_grouping = \"separate\"\n\
         tool_preview_length = 12\ninterim_assistant_messages = false\n",
    )))
    .await;
    h.dm("discord", "1", "tools:2").await;
    said(&h.adapter("discord"), "tools done").await;
    let lines = progress_sends(&h, "discord");
    assert_eq!(
        lines,
        [
            "\u{2699} terminal: ls /tmp/0 s…",
            "\u{2699} terminal: ls /tmp/1 s…"
        ],
        "twelve characters each"
    );
    assert!(
        !h.adapter("discord")
            .texts()
            .contains(&"Let me look.".to_owned())
    );
}

#[tokio::test]
async fn a_failed_run_keeps_its_progress_and_a_successful_one_cleans_up() {
    let mut setup = Setup::new(&both(
        "[gateway.display.platforms.discord]\ncleanup_progress = true\n",
    ));
    setup.descriptors.push(deletable("discord"));
    let h = Harness::start(setup).await;
    h.dm("discord", "1", "tools:2:fail").await;
    said(&h.adapter("discord"), "The turn failed").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h.adapter("discord").deleted.lock().unwrap().is_empty());
    h.dm("discord", "1", "tools:2").await;
    said(&h.adapter("discord"), "tools done").await;
    eventually("the bubbles are deleted", || {
        h.adapter("discord").deleted.lock().unwrap().len() == 2
    })
    .await;
}

#[tokio::test]
async fn log_mode_writes_redacted_calls_instead_of_chat_lines() {
    let h = Harness::start(Setup::new(&both(
        "[gateway.display]\ntool_progress = \"log\"\n",
    )))
    .await;
    h.dm("discord", "1", "tools:2").await;
    said(&h.adapter("discord"), "tools done").await;
    assert!(progress_sends(&h, "discord").is_empty());
    let log = std::fs::read_to_string(h.home.path().join("logs/default/tool_calls.log")).unwrap();
    assert_eq!(log.lines().count(), 2);
    assert!(log.contains("ls /tmp/0") && !log.contains("sk-proj-abcdefghijklmnopqrstuvwxyz"));
}

#[tokio::test]
async fn verbose_cycles_the_chats_mode_only_where_the_policy_allows() {
    let h = Harness::start(Setup::new(&both(
        "[gateway.display.platforms.telegram]\ntool_progress_command = true\n",
    )))
    .await;
    h.dm("discord", "1", "/verbose").await;
    said(&h.adapter("discord"), "set by the display policy").await;
    h.dm("telegram", "1", "/verbose").await;
    said(&h.adapter("telegram"), "Tool progress: new").await;
    h.dm("telegram", "1", "tools:2").await;
    said(&h.adapter("telegram"), "tools done").await;
    assert_eq!(progress_sends(&h, "telegram").len(), 1);
}

#[tokio::test]
async fn the_footer_ends_final_replies_where_the_chat_turned_it_on() {
    let h = Harness::start(Setup::new(&open("slack"))).await;
    *h.agents.cwd.lock().unwrap() = Some("/work/acme-api".into());
    h.dm("slack", "1", "/footer status").await;
    said(&h.adapter("slack"), "Footer: off (display policy)").await;
    h.dm("slack", "1", "/footer on").await;
    said(&h.adapter("slack"), "Footer on for this chat.").await;
    h.dm("slack", "1", "hello").await;
    said(&h.adapter("slack"), "echo: hello").await;
    let reply = h
        .adapter("slack")
        .texts()
        .into_iter()
        .find(|t| t.starts_with("echo: hello"))
        .unwrap();
    // The fake platform has no italics: the line is plain.
    assert!(reply.ends_with("\n\nacme-api"), "{reply}");
    assert!(
        !h.agents.texts().iter().any(|t| t.contains("acme-api")),
        "the agent never sees it"
    );
}

#[tokio::test]
async fn a_suppressed_warning_is_settled_without_reaching_its_platform() {
    let h = Harness::start(Setup::new(&both(
        "[gateway.display]\nsuppress_warning_notifications = true\n\
         [gateway.display.platforms.telegram]\nsuppress_warning_notifications = false\n",
    )))
    .await;
    for target in ["slack", "telegram"] {
        let request = SendRequest {
            target: target.into(),
            text: "Job nightly-report failed: exit 1".into(),
            attachments: Vec::new(),
            warning: true,
        };
        h.gateway.send("default", &request).await.unwrap();
    }
    said(&h.adapter("telegram"), "Job nightly-report failed").await;
    let store = h.store();
    eventually("the slack entry settles", || {
        store
            .outbox_entries(Some(OutboundState::Suppressed), 10)
            .unwrap()
            .iter()
            .any(|e| e.platform == "slack")
    })
    .await;
    assert!(h.adapter("slack").texts().is_empty());
}

#[tokio::test]
async fn unaddressed_silence_sends_nothing_and_addressed_silence_gets_a_notice() {
    let h = Harness::start(Setup::new(&open("slack"))).await;
    let mut chatter = Inbound::text(ChatType::Channel, "C1", "u1", "say:[SILENT]");
    chatter.addressing.not_addressed = true;
    chatter.message_id = "m1".into();
    h.send("slack", chatter).await;
    let store = h.store();
    eventually("the silent reply is recorded", || {
        store
            .outbox_entries(Some(OutboundState::Silent), 10)
            .unwrap()
            .iter()
            .any(|e| e.body == "[SILENT]")
    })
    .await;
    assert!(h.adapter("slack").texts().is_empty());
    h.dm("slack", "u2", "say:NO_REPLY").await;
    said(&h.adapter("slack"), "(nothing to add)").await;
    h.dm("slack", "u3", "say:I have [SILENT] thoughts").await;
    said(&h.adapter("slack"), "I have [SILENT] thoughts").await;
}

#[tokio::test]
async fn a_detailed_busy_acknowledgement_names_what_the_turn_does() {
    let h = Harness::start(Setup::new(&both(
        "[gateway.display]\nbusy_ack_detail = true\n",
    )))
    .await;
    h.dm("discord", "1", "slow:500").await;
    eventually("the turn runs", || h.agents.running() == 1).await;
    h.dm("discord", "1", "and another thing").await;
    said(
        &h.adapter("discord"),
        "this one is next. (thinking, 0 min in)",
    )
    .await;
}

/// Spec: messaging-gateway, Concurrency caps across surfaces: a turn the
/// daemon holds back for a free slot is announced in its chat.
#[tokio::test]
async fn a_turn_waiting_for_a_slot_is_announced() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.dm("telegram", "1", "wait:2").await;
    said(&h.adapter("telegram"), "Waiting for a free slot (2 ahead)").await;
    said(&h.adapter("telegram"), "echo: wait:2").await;
}
