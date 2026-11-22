//! What notifies and what a platform is told about a turn (spec:
//! platform-adapters, Telegram presentation options): final replies,
//! approvals and command confirmations are marked notifying, progress is
//! not, and the adapter hears when the turn its message started begins and
//! ends.

use pmt_gateway::messaging::{ChatType, Inbound, Outgoing};

use crate::support::{Did, Harness, Setup, config, eventually, said};

fn sent(h: &Harness, containing: &str) -> Vec<Outgoing> {
    h.adapter("telegram")
        .all()
        .into_iter()
        .filter_map(|d| match d {
            Did::Send(m) if m.text.contains(containing) => Some(m),
            _ => None,
        })
        .collect()
}

/// Scenario: Progress is silent (the gateway's half: what it marks).
#[tokio::test]
async fn only_the_final_reply_of_a_turn_is_marked_notifying() {
    let h = Harness::start(Setup::new(&config(
        "telegram",
        "allow_all = true\n\n[gateway.display.platforms.telegram]\n\
         tool_progress = \"all\"\ntool_progress_grouping = \"separate\"\n",
    )))
    .await;
    h.dm("telegram", "1", "tools:2").await;
    said(&h.adapter("telegram"), "tools done").await;
    let progress = sent(&h, "\u{2699} terminal");
    assert_eq!(progress.len(), 2, "two status updates");
    assert!(progress.iter().all(|m| m.notify.is_none()), "{progress:?}");
    let replies = sent(&h, "tools done");
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].notify, Some(true));
}

#[tokio::test]
async fn approvals_and_command_confirmations_notify() {
    let h = Harness::start(Setup::new(&config("telegram", "allow_all = true"))).await;
    h.dm("telegram", "1", "/stop").await;
    said(&h.adapter("telegram"), "No active task to stop.").await;
    assert_eq!(sent(&h, "No active task")[0].notify, Some(true));
    h.dm("telegram", "1", "approval").await;
    eventually("the card is sent", || {
        h.adapter("telegram")
            .all()
            .iter()
            .any(|d| matches!(d, Did::Send(m) if !m.buttons.is_empty()))
    })
    .await;
    let card = h
        .adapter("telegram")
        .all()
        .into_iter()
        .find_map(|d| match d {
            Did::Send(m) if !m.buttons.is_empty() => Some(m),
            _ => None,
        })
        .unwrap();
    assert_eq!(card.notify, Some(true));
}

#[tokio::test]
async fn the_adapter_hears_when_the_turn_of_a_message_starts_and_ends() {
    let h = Harness::start(Setup::new(&config("telegram", "allow_all = true"))).await;
    let mut inbound = Inbound::text(ChatType::Dm, "1", "1", "say:hello");
    inbound.message_id = "m-7".to_owned();
    h.send("telegram", inbound).await;
    said(&h.adapter("telegram"), "hello").await;
    eventually("the turn ended", || {
        h.adapter("telegram")
            .turns
            .lock()
            .unwrap()
            .contains(&"turn m-7 ended".to_owned())
    })
    .await;
    let turns = h.adapter("telegram").turns.lock().unwrap().clone();
    assert_eq!(turns, ["turn m-7 started", "turn m-7 ended"]);
}

#[tokio::test]
async fn a_status_message_is_edited_in_place_and_resent_when_the_edit_fails() {
    let h = Harness::start(Setup::new(&config(
        "telegram",
        "allow_all = true\n\n[gateway.display.platforms.telegram]\ntool_progress = \"all\"\n",
    )))
    .await;
    h.dm("telegram", "1", "tools:2").await;
    said(&h.adapter("telegram"), "tools done").await;
    assert_eq!(sent(&h, "\u{2699} terminal").len(), 1, "one bubble, edited");
    let adapter = h.adapter("telegram");
    adapter
        .edit_unsupported
        .store(true, std::sync::atomic::Ordering::Relaxed);
    h.dm("telegram", "1", "tools:2").await;
    eventually("the second reply", || sent(&h, "tools done").len() == 2).await;
    let bubbles = sent(&h, "\u{2699} terminal");
    assert_eq!(bubbles.len(), 3, "the failed edit became a new message");
    assert_eq!(bubbles[2].text.matches("\u{2699} terminal").count(), 2);
}
