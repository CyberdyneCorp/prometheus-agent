//! Chat controls (spec: messaging-gateway, Platform start pings and
//! paginated command browser; Text approval and busy-mode commands;
//! Discarding a restart-interrupted turn; Session stall watchdog and
//! reconnect attention).

use std::time::Duration;

use pmt_gateway::messaging::{
    AdapterError, AdapterState, Admission, ChatType, Descriptor, Inbound, Interactive, MenuCommand,
    RESTARTED,
};

use crate::support::{Did, Harness, Setup, config, eventually, open, said};

fn menu(n: usize) -> Vec<MenuCommand> {
    (0..n)
        .map(|i| MenuCommand {
            name: format!("cmd{i:02}"),
            description: format!("command {i}"),
            takes_args: false,
        })
        .collect()
}

#[tokio::test]
async fn a_start_ping_greets_without_a_session_or_a_rate_limit_count() {
    let h = Harness::start(Setup::new(&config(
        "telegram",
        "allow_all = true\nrate_limit = { burst = 2, messages_per_minute = 2 }",
    )))
    .await;
    for _ in 0..4 {
        let admission = h.dm("telegram", "1", "/start").await;
        assert_eq!(admission, Admission::Handled);
    }
    said(&h.adapter("telegram"), "/help").await;
    assert!(h.sessions().is_empty(), "a start ping creates no session");
    assert!(h.agents.texts().is_empty(), "nor a turn");
    let admission = h.dm("telegram", "1", "hello").await;
    assert_eq!(admission, Admission::Accepted, "pings were not counted");
}

#[tokio::test]
async fn a_page_past_the_last_names_the_page_count() {
    let h = Harness::start(Setup::new(&open("discord"))).await;
    *h.agents.menu.lock().unwrap() = menu(37);
    h.dm("discord", "1", "/commands 3").await;
    said(&h.adapter("discord"), "page 3 does not exist (2 pages)").await;
}

#[tokio::test]
async fn paging_buttons_edit_the_listing_in_place() {
    let h = Harness::start(Setup::new(&open("discord"))).await;
    *h.agents.menu.lock().unwrap() = menu(37);
    h.dm("discord", "1", "/commands").await;
    said(&h.adapter("discord"), "/cmd19 - command 19").await;
    let first = h.adapter("discord").all();
    let Some(Did::Send(listing)) = first.last() else {
        panic!("no listing: {first:?}");
    };
    assert!(!listing.text.contains("/cmd20"));
    assert_eq!(listing.buttons.len(), 1, "only Next on the first page");
    let mut press = Inbound::text(ChatType::Dm, "1", "1", "");
    press.callback = Some(listing.buttons[0].data.clone());
    h.send("discord", press).await;
    eventually("the listing is edited to page 2", || {
        h.adapter("discord").all().iter().any(|d| {
            matches!(d, Did::Edit(_, m) if m.text.contains("/cmd36") && !m.text.contains("/cmd19"))
        })
    })
    .await;
}

#[tokio::test]
async fn without_buttons_a_page_carries_its_footer_and_telegram_shows_fifteen() {
    let mut setup = Setup::new(&open("telegram"));
    setup.descriptors.push(Descriptor {
        interactive: Interactive::Numbered,
        ..Descriptor::new("telegram", 4096)
    });
    let h = Harness::start(setup).await;
    *h.agents.menu.lock().unwrap() = menu(37);
    h.dm("telegram", "1", "/commands 2").await;
    said(&h.adapter("telegram"), "page 2/3").await;
    let text = h.adapter("telegram").texts().pop().unwrap();
    assert!(text.contains("/cmd15") && text.contains("/cmd29") && !text.contains("/cmd30"));
}

#[tokio::test]
async fn the_command_list_follows_the_senders_tier() {
    let h = Harness::start(Setup::new(&config(
        "discord",
        "allow_all = true\nadmins = [\"111\"]",
    )))
    .await;
    let mut commands = menu(2);
    for name in ["help", "stop", "model"] {
        commands.push(MenuCommand {
            name: name.into(),
            description: String::new(),
            takes_args: false,
        });
    }
    *h.agents.menu.lock().unwrap() = commands;
    h.dm("discord", "222", "/commands").await;
    said(&h.adapter("discord"), "/help").await;
    let text = h.adapter("discord").texts().pop().unwrap();
    assert!(text.contains("/stop") && !text.contains("/model") && !text.contains("/cmd00"));
}

#[tokio::test]
async fn approve_session_by_text_resolves_the_oldest_pending_approval() {
    let mut setup = Setup::new(&open("signal"));
    setup.no_edit.push("signal");
    let h = Harness::start(setup).await;
    h.dm("signal", "1", "/approve").await;
    said(&h.adapter("signal"), "No pending approval").await;
    h.dm("signal", "1", "approval").await;
    said(&h.adapter("signal"), "Approval needed: terminal").await;
    h.dm("signal", "1", "/approve session").await;
    said(&h.adapter("signal"), "Approved for this session by 1").await;
    said(&h.adapter("signal"), "approved").await;
}

#[tokio::test]
async fn busy_mode_is_persisted_for_the_platform_and_reported_with_its_source() {
    let h = Harness::start(Setup::new(&config(
        "telegram",
        "allow_all = true\nadmins = [\"1\"]",
    )))
    .await;
    h.dm("telegram", "1", "/busy status").await;
    said(&h.adapter("telegram"), "queue (busy.input_mode)").await;
    h.dm("telegram", "2", "/busy steer").await;
    said(&h.adapter("telegram"), "restricted to admins").await;
    h.dm("telegram", "1", "/busy steer").await;
    said(&h.adapter("telegram"), "is now steer").await;
    let scope = pmt_gateway::messaging::Agents::scope(h.agents.as_ref(), "default").unwrap();
    assert_eq!(
        scope.config().gateway.platform("telegram").busy.input_mode,
        Some(pmt_config::schema::BusyInputMode::Steer),
        "written to config.toml"
    );
    h.dm("telegram", "1", "/busy status").await;
    said(&h.adapter("telegram"), "steer (platform override)").await;
    // The next message mid-turn steers.
    h.dm("telegram", "1", "slow:400").await;
    eventually("the turn runs", || h.agents.running() == 1).await;
    h.dm("telegram", "1", "also this").await;
    eventually("it steered", || {
        h.agents.steered.lock().unwrap().as_slice() == ["also this"]
    })
    .await;
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Runs `phase` on a fresh runtime, then drops it: a crash unless the
/// phase stopped the gateway.
fn phase<T>(phase: impl std::future::Future<Output = T>) -> T {
    let rt = runtime();
    let out = rt.block_on(phase);
    drop(rt);
    out
}

/// A home whose only session's last turn a crash left `resume_pending`.
fn crashed_mid_turn(config: &str) -> tempfile::TempDir {
    phase(async {
        let h = Harness::start(Setup::new(config)).await;
        h.dm("telegram", "1", "slow:60000").await;
        eventually("the turn runs", || h.agents.running() == 1).await;
        h.home
    })
}

#[test]
fn discard_marks_the_pending_turn_and_runs_the_queue_in_order() {
    let home = crashed_mid_turn(&open("telegram"));
    phase(async {
        let h = Harness::start_in(home, Setup::new(&open("telegram"))).await;
        said(&h.adapter("telegram"), RESTARTED).await;
        h.dm("telegram", "1", "first").await;
        said(&h.adapter("telegram"), "reply /retry").await;
        h.dm("telegram", "1", "second").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(h.agents.texts().is_empty(), "the queue waits for a choice");
        h.dm("telegram", "1", "/discard").await;
        said(&h.adapter("telegram"), "Dropped the interrupted turn.").await;
        eventually("both queued messages ran", || h.agents.texts().len() == 1).await;
        said(&h.adapter("telegram"), "echo: first\nsecond").await;
        let session = h.sessions()[0];
        let state = h.store().routed_session(session).unwrap().unwrap();
        assert!(
            !state.resume_pending,
            "the turn is interrupted, not pending"
        );
        h.dm("telegram", "1", "/discard").await;
        said(&h.adapter("telegram"), "No interrupted turn to discard.").await;
    });
}

#[test]
fn retry_runs_the_pending_turn_before_the_waiting_input() {
    let home = crashed_mid_turn(&open("telegram"));
    phase(async {
        let h = Harness::start_in(home, Setup::new(&open("telegram"))).await;
        said(&h.adapter("telegram"), RESTARTED).await;
        h.dm("telegram", "1", "later").await;
        said(&h.adapter("telegram"), "reply /retry").await;
        h.dm("telegram", "1", "/retry").await;
        eventually("the retried turn, then the queue", || {
            h.agents.texts() == ["the last message again", "later"]
        })
        .await;
    });
}

#[test]
fn an_unanswered_pending_turn_is_dropped_after_its_ttl() {
    let config = format!(
        "{}\n[gateway.sessions]\nresume_pending_ttl_secs = 1\n",
        open("telegram")
    );
    let home = crashed_mid_turn(&config);
    phase(async {
        let h = Harness::start_in(home, Setup::new(&config)).await;
        said(&h.adapter("telegram"), RESTARTED).await;
        h.dm("telegram", "1", "waiting").await;
        said(&h.adapter("telegram"), "echo: waiting").await;
        said(&h.adapter("telegram"), "dropped after 1 s").await;
    });
}

#[tokio::test]
async fn a_stalled_turn_is_noticed_once_then_interrupted() {
    let config = format!(
        "[gateway]\nsession_stall_timeout_secs = 1\n\n{}",
        open("telegram")
    );
    let h = Harness::start(Setup::new(&config)).await;
    h.dm("telegram", "1", "slow:60000").await;
    said(
        &h.adapter("telegram"),
        "No progress for 1 s; send /stop to cancel",
    )
    .await;
    said(
        &h.adapter("telegram"),
        "The turn was interrupted as stalled: no progress for 2 s.",
    )
    .await;
    eventually("the turn ended", || h.agents.running() == 0).await;
    let notices = h
        .adapter("telegram")
        .texts()
        .iter()
        .filter(|t| t.starts_with("No progress"))
        .count();
    assert_eq!(notices, 1);
}

#[tokio::test]
async fn a_long_outage_is_escalated_once_to_the_other_home_channel() {
    let config = "[gateway]\nreconnect_attention_secs = 1\n\n\
        [gateway.busy]\ndebounce_ms = 0\n\n\
        [gateway.platforms.telegram]\nenabled = true\nallow_all = true\n\
        home_channel = { chat_id = \"home\" }\n\n\
        [gateway.platforms.discord]\nenabled = true\nallow_all = true\n";
    let mut setup = Setup::new(config);
    setup.connect_errors.push((
        ("default".into(), "discord".into()),
        AdapterError {
            retryable: true,
            code: "network".into(),
            message: "unreachable".into(),
        },
    ));
    let h = Harness::start(setup).await;
    said(&h.adapter("telegram"), "Discord has been unavailable for").await;
    let status = h
        .gateway
        .statuses()
        .into_iter()
        .find(|s| s.platform == "discord")
        .unwrap();
    assert!(status.attention);
    assert_eq!(status.state, AdapterState::Retrying);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let notices: Vec<_> = h
        .adapter("telegram")
        .all()
        .into_iter()
        .filter_map(|d| match d {
            Did::Send(m) if m.text.contains("Discord has been") => Some(m.chat_id),
            _ => None,
        })
        .collect();
    assert_eq!(notices, ["home"], "one notice, in the home channel");
}
