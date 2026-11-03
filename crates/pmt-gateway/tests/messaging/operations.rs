//! Adapter operations, restart and update from a chat (spec: messaging-
//! gateway, Adapter operations from chat; Restart and update from chat).
//! Every operation is admin-tier, and a denied confirmation does nothing.

use std::time::{Duration, Instant};

use pmt_gateway::messaging::{AdapterError, AdapterState, Admission, ChatType, Inbound};

use crate::support::{Did, Harness, Setup, eventually, said};

/// Telegram (admin `1`) and Discord, both admitting everyone.
fn two_platforms(extra: &str) -> String {
    format!(
        "[gateway.busy]\ndebounce_ms = 0\n\n\
         [gateway.platforms.telegram]\nenabled = true\nallow_all = true\nadmins = [\"1\"]\n\
         home_channel = {{ chat_id = \"home\" }}\n\n\
         [gateway.platforms.discord]\nenabled = true\nallow_all = true\n{extra}"
    )
}

fn status_of(h: &Harness, platform: &str) -> pmt_gateway::messaging::AdapterStatus {
    h.gateway
        .statuses()
        .into_iter()
        .find(|s| s.platform == platform)
        .unwrap()
}

/// Presses the button labelled `label` on the last card the adapter got.
async fn press(h: &Harness, platform: &str, chat: ChatType, sender: &str, label: &str) {
    let card = h
        .adapter(platform)
        .all()
        .into_iter()
        .rev()
        .find_map(|d| match d {
            Did::Send(m) if !m.buttons.is_empty() => Some(m),
            _ => None,
        })
        .expect("a card with buttons");
    let button = card.buttons.iter().find(|b| b.label == label).unwrap();
    let mut inbound = Inbound::text(chat, &card.chat_id, sender, "");
    inbound.thread_id = card.thread_id.clone();
    inbound.callback = Some(button.data.clone());
    h.send(platform, inbound).await;
}

#[tokio::test]
async fn resume_clears_the_backoff_and_reconnects_at_once() {
    let mut setup = Setup::new(&two_platforms(""));
    setup.connect_errors.push((
        ("default".into(), "discord".into()),
        AdapterError {
            retryable: true,
            code: "network".into(),
            message: "unreachable".into(),
        },
    ));
    let h = Harness::start(setup).await;
    // Two failures: the next attempt is seconds away.
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert_eq!(status_of(&h, "discord").state, AdapterState::Retrying);
    *h.adapter("discord").connect_error.lock().unwrap() = None;
    let asked = Instant::now();
    h.dm("telegram", "1", "/platform resume discord").await;
    said(&h.adapter("telegram"), "Resumed. discord: degraded").await;
    assert!(
        asked.elapsed() < Duration::from_millis(1500),
        "reconnected without waiting for the backoff"
    );
    assert!(h.adapter("discord").link.lock().unwrap().is_some());
}

#[tokio::test]
async fn a_paused_adapter_drops_its_messages_and_stays_paused_across_restarts() {
    let h = Harness::start(Setup::new(&two_platforms(""))).await;
    h.dm("telegram", "2", "/platform pause discord").await;
    said(&h.adapter("telegram"), "restricted to admins").await;
    h.dm("telegram", "1", "/platform pause discord").await;
    said(&h.adapter("telegram"), "Paused discord").await;
    assert_eq!(h.dm("discord", "9", "hello").await, Admission::Paused);
    assert!(h.agents.texts().is_empty());
    assert!(status_of(&h, "discord").paused);
    h.dm("telegram", "1", "/platforms").await;
    said(
        &h.adapter("telegram"),
        "discord: degraded (unconfirmed) [paused]",
    )
    .await;
    let home = h.stop().await;
    let h = Harness::start_in(home, Setup::new(&two_platforms(""))).await;
    assert_eq!(h.dm("discord", "9", "hello").await, Admission::Paused);
    h.dm("telegram", "1", "/platform resume discord").await;
    said(&h.adapter("telegram"), "Resumed. discord").await;
    assert_eq!(h.dm("discord", "9", "hello").await, Admission::Accepted);
    said(&h.adapter("discord"), "echo: hello").await;
}

#[tokio::test]
async fn a_restart_confirmed_in_a_thread_is_announced_there_by_the_next_daemon() {
    let config = "[gateway.busy]\ndebounce_ms = 0\n\n\
        [gateway.platforms.slack]\nenabled = true\nallow_all = true\n\
        group_admins = [\"boss\"]\n";
    let h = Harness::start(Setup::new(config)).await;
    let mut asked = Inbound::text(ChatType::Thread, "C1", "boss", "/restart");
    asked.thread_id = Some("T1".into());
    h.send("slack", asked).await;
    said(&h.adapter("slack"), "Restart the gateway daemon?").await;
    press(&h, "slack", ChatType::Thread, "boss", "Confirm").await;
    said(&h.adapter("slack"), "Restarting the gateway now.").await;
    assert!(h.server.restart_requested());
    let home = h.stop().await;
    let h = Harness::start_in(home, Setup::new(config)).await;
    said(&h.adapter("slack"), "Gateway restarted (0.0.0, build test)").await;
    let announced = h
        .adapter("slack")
        .all()
        .into_iter()
        .find_map(|d| match d {
            Did::Send(m) if m.text.starts_with("Gateway restarted") => Some(m),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        (announced.chat_id.as_str(), announced.thread_id.as_deref()),
        ("C1", Some("T1"))
    );
}

#[tokio::test]
async fn a_denied_restart_does_nothing() {
    let h = Harness::start(Setup::new(&two_platforms(""))).await;
    h.dm("telegram", "1", "/restart").await;
    said(&h.adapter("telegram"), "Restart the gateway daemon?").await;
    press(&h, "telegram", ChatType::Dm, "1", "Cancel").await;
    eventually("the card says it was aborted", || {
        h.adapter("telegram").all().iter().any(
            |d| matches!(d, Did::Edit(_, m) if m.text == "restart aborted: confirmation denied"),
        )
    })
    .await;
    assert!(!h.server.restart_requested());
}

#[tokio::test]
async fn update_from_chat_is_off_by_default() {
    let h = Harness::start(Setup::new(&two_platforms(""))).await;
    h.dm("telegram", "1", "/update").await;
    said(
        &h.adapter("telegram"),
        "updates from chat are disabled (update.allow_from_chat = false)",
    )
    .await;
    assert!(h.agents.updates.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_denied_update_confirmation_aborts_without_running_the_updater() {
    let h = Harness::start(Setup::new(&two_platforms(""))).await;
    std::fs::write(
        h.home.path().join("daemon.toml"),
        "[update]\nallow_from_chat = true\n",
    )
    .unwrap();
    h.dm("telegram", "2", "/update").await;
    said(&h.adapter("telegram"), "restricted to admins").await;
    h.dm("telegram", "1", "/update").await;
    said(&h.adapter("telegram"), "Update prometheus now?").await;
    h.dm("telegram", "1", "/deny").await;
    eventually("the card says it was aborted", || {
        h.adapter("telegram").all().iter().any(
            |d| matches!(d, Did::Edit(_, m) if m.text == "update aborted: confirmation denied"),
        )
    })
    .await;
    assert!(h.agents.updates.lock().unwrap().is_empty());
    h.dm("telegram", "1", "/update").await;
    eventually("a second card", || {
        h.adapter("telegram")
            .texts()
            .iter()
            .filter(|t| t.starts_with("Update prometheus now?"))
            .count()
            == 2
    })
    .await;
    press(&h, "telegram", ChatType::Dm, "1", "Confirm").await;
    said(&h.adapter("telegram"), "update started").await;
    let updates = h.agents.updates.lock().unwrap().clone();
    assert_eq!(updates.len(), 1);
    assert!(!updates[0].check);
}

#[tokio::test]
async fn a_restart_that_cut_turns_short_tells_each_home_channel_once() {
    let mut setup = Setup::new(&two_platforms(""));
    setup.drain = Duration::from_millis(100);
    let h = Harness::start(setup).await;
    h.dm("discord", "5", "slow:60000").await;
    eventually("the turn runs", || h.agents.running() == 1).await;
    h.server.request_stop(true);
    let home = h.stop().await;
    let h = Harness::start_in(home, Setup::new(&two_platforms(""))).await;
    said(
        &h.adapter("telegram"),
        "The gateway restarted while turns were running",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let notices: Vec<String> = h
        .adapter("telegram")
        .all()
        .into_iter()
        .filter_map(|d| match d {
            Did::Send(m) if m.text.starts_with("The gateway restarted") => Some(m.chat_id),
            _ => None,
        })
        .collect();
    assert_eq!(notices, ["home"]);
}
