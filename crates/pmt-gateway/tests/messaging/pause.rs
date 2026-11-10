//! The global emergency pause (spec: messaging-gateway, Global emergency
//! pause).

use pmt_gateway::messaging::{ChatType, Inbound};

use crate::support::{Harness, Setup, config, eventually, open, said};

const NOTICE: &str =
    "Paused: billing incident. Run `prometheus pause off` (or /pause off) to resume.";

#[tokio::test]
async fn a_message_during_a_pause_starts_no_turn() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    pmt_core::pause::pause(h.home.path(), Some("billing incident"), "cli:owner").unwrap();
    h.dm("telegram", "42", "summarize my inbox").await;
    said(&h.adapter("telegram"), NOTICE).await;
    assert!(h.agents.texts().is_empty(), "no turn started");
    h.dm("telegram", "42", "/whoami").await;
    said(&h.adapter("telegram"), "You are 42").await;
    pmt_core::pause::resume(h.home.path()).unwrap();
    h.dm("telegram", "42", "now?").await;
    said(&h.adapter("telegram"), "echo: now?").await;
}

#[tokio::test]
async fn an_in_flight_turn_finishes_and_the_next_message_is_refused() {
    let h = Harness::start(Setup::new(&config(
        "discord",
        "allow_all = true\ngroup_admins = [\"boss\"]",
    )))
    .await;
    let from = |sender: &str, text: &str| Inbound::text(ChatType::Group, "c", sender, text);
    h.send("discord", from("boss", "slow:600:the long answer"))
        .await;
    eventually("the turn runs", || h.agents.running() == 1).await;
    h.send("discord", from("boss", "/pause billing incident"))
        .await;
    said(&h.adapter("discord"), "Paused: billing incident.").await;
    said(&h.adapter("discord"), "the long answer").await;
    h.send("discord", from("boss", "next question")).await;
    said(&h.adapter("discord"), NOTICE).await;
    assert_eq!(h.agents.texts(), vec!["slow:600:the long answer"]);
    h.send("discord", from("boss", "/pause status")).await;
    said(&h.adapter("discord"), "by discord:boss: billing incident").await;
    h.send("discord", from("boss", "/pause off")).await;
    said(&h.adapter("discord"), "Pause lifted").await;
}

#[tokio::test]
async fn only_admins_pause_where_an_admin_list_is_set() {
    let h = Harness::start(Setup::new(&config(
        "telegram",
        "allow_all = true\nadmins = [\"111\"]",
    )))
    .await;
    h.dm("telegram", "222", "/pause").await;
    said(
        &h.adapter("telegram"),
        "This command is restricted to admins.",
    )
    .await;
    assert!(pmt_core::pause::active(h.home.path()).is_none());
}

#[tokio::test]
async fn a_pause_survives_a_restart() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.dm("telegram", "1", "/pause billing incident").await;
    said(&h.adapter("telegram"), "Paused: billing incident.").await;
    let home = h.stop().await;
    let h = Harness::start_in(home, Setup::new(&open("telegram"))).await;
    // What every scheduler checks before it fires.
    assert!(pmt_core::pause::check(h.home.path()).is_err());
    h.dm("telegram", "1", "hello").await;
    said(&h.adapter("telegram"), NOTICE).await;
    assert!(h.agents.texts().is_empty());
}

/// Spec: model-providers, Subscription account commands, "Login refused in
/// a group": a sign-in code never goes to a group chat.
#[tokio::test]
async fn login_in_a_group_is_refused_and_points_to_a_direct_message() {
    let h = Harness::start(Setup::new(&config("telegram", "allow_all = true"))).await;
    h.send(
        "telegram",
        Inbound::text(ChatType::Group, "team", "42", "/login"),
    )
    .await;
    said(&h.adapter("telegram"), pmt_gateway::messaging::LOGIN_IN_DM).await;
    assert!(
        h.agents.texts().is_empty(),
        "no turn and no sign-in started"
    );
}
