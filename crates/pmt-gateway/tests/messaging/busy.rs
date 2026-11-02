//! Busy input (spec: messaging-gateway, Busy input policy from
//! configuration only; Command busy policy declared in the registry;
//! Queued turns run in FIFO order; Stop preserves the session).

use pmt_gateway::messaging::Admission;

use crate::support::{Harness, Setup, config, eventually, said};

fn with_mode(mode: &str) -> String {
    config(
        "telegram",
        &format!("allow_all = true\n[gateway.platforms.telegram.busy]\ninput_mode = \"{mode}\""),
    )
}

#[tokio::test]
async fn steer_reaches_the_running_turn_without_a_second_turn() {
    let h = Harness::start(Setup::new(&with_mode("steer"))).await;
    h.dm("telegram", "1", "slow:800").await;
    eventually("the turn runs", || h.agents.running() == 1).await;
    let admission = h
        .dm("telegram", "1", "use the staging database instead")
        .await;
    assert_eq!(admission, Admission::Queued);
    eventually("the text is steered", || {
        !h.agents.steered.lock().unwrap().is_empty()
    })
    .await;
    said(&h.adapter("telegram"), "slow done").await;
    assert_eq!(
        h.agents.steered.lock().unwrap().clone(),
        vec!["use the staging database instead"]
    );
    assert_eq!(h.agents.texts().len(), 1, "no second turn");
}

#[tokio::test]
async fn three_follow_ups_get_one_acknowledgement_and_one_merged_turn() {
    let h = Harness::start(Setup::new(&with_mode("queue"))).await;
    h.dm("telegram", "1", "slow:500").await;
    eventually("the turn runs", || h.agents.running() == 1).await;
    for text in ["one", "two", "three"] {
        h.dm("telegram", "1", text).await;
    }
    said(&h.adapter("telegram"), "echo: one\ntwo\nthree").await;
    let acks = h
        .adapter("telegram")
        .texts()
        .iter()
        .filter(|t| t.starts_with("Still working"))
        .count();
    assert_eq!(acks, 1);
    assert_eq!(h.agents.texts(), vec!["slow:500", "one\ntwo\nthree"]);
}

#[tokio::test]
async fn interrupt_stops_the_turn_and_runs_the_new_input() {
    let h = Harness::start(Setup::new(&with_mode("interrupt"))).await;
    h.dm("telegram", "1", "slow:10000").await;
    eventually("the turn runs", || h.agents.running() == 1).await;
    h.dm("telegram", "1", "never mind, do this").await;
    said(&h.adapter("telegram"), "echo: never mind, do this").await;
    assert_eq!(h.agents.texts(), vec!["slow:10000", "never mind, do this"]);
}

#[tokio::test]
async fn approve_runs_at_once_while_the_turn_waits_for_it() {
    let h = Harness::start(Setup::new(&with_mode("queue"))).await;
    h.dm("telegram", "1", "approval").await;
    said(&h.adapter("telegram"), "Approval needed: terminal").await;
    h.dm("telegram", "1", "/approve").await;
    said(&h.adapter("telegram"), "approved").await;
    assert_eq!(
        h.agents.texts(),
        vec!["approval"],
        "/approve was not queued as a turn"
    );
}

#[tokio::test]
async fn a_reject_command_mid_turn_is_refused_and_the_turn_continues() {
    let h = Harness::start(Setup::new(&with_mode("queue"))).await;
    h.dm("telegram", "1", "slow:400").await;
    eventually("the turn runs", || h.agents.running() == 1).await;
    h.dm("telegram", "1", "/model gpt-x").await;
    said(
        &h.adapter("telegram"),
        "Agent is running - /model can't run mid-turn. Wait for the response or /stop first.",
    )
    .await;
    said(&h.adapter("telegram"), "slow done").await;
    assert!(h.agents.commands.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unknown_slash_text_mid_turn_is_ordinary_input() {
    let h = Harness::start(Setup::new(&with_mode("queue"))).await;
    h.dm("telegram", "1", "slow:300").await;
    eventually("the turn runs", || h.agents.running() == 1).await;
    h.dm("telegram", "1", "/notacommand hello").await;
    said(&h.adapter("telegram"), "echo: /notacommand hello").await;
    assert_eq!(h.agents.texts(), vec!["slow:300", "/notacommand hello"]);
}

#[tokio::test]
async fn queued_prompts_run_as_separate_turns_in_order() {
    let h = Harness::start(Setup::new(&with_mode("queue"))).await;
    h.dm("telegram", "1", "slow:300").await;
    eventually("the turn runs", || h.agents.running() == 1).await;
    h.dm("telegram", "1", "/queue A").await;
    h.dm("telegram", "1", "/queue B").await;
    said(&h.adapter("telegram"), "echo: B").await;
    assert_eq!(h.agents.texts(), vec!["slow:300", "A", "B"]);
}

#[tokio::test]
async fn the_queue_is_bounded() {
    let config = with_mode("queue").replace("debounce_ms = 0", "debounce_ms = 0\nmax_queued = 2");
    let h = Harness::start(Setup::new(&config)).await;
    h.dm("telegram", "1", "slow:2000").await;
    eventually("the turn runs", || h.agents.running() == 1).await;
    for text in ["/queue A", "/queue B", "/queue C"] {
        h.dm("telegram", "1", text).await;
    }
    said(&h.adapter("telegram"), "Queue full (2). Wait or /stop.").await;
}

#[tokio::test]
async fn stop_keeps_the_session_for_the_next_message() {
    let h = Harness::start(Setup::new(&with_mode("queue"))).await;
    h.dm("telegram", "1", "slow:10000").await;
    eventually("the turn runs", || h.agents.running() == 1).await;
    h.dm("telegram", "1", "queued follow-up").await;
    h.dm("telegram", "1", "/stop").await;
    said(
        &h.adapter("telegram"),
        "Stopped. You can continue this session.",
    )
    .await;
    eventually("the turn ends", || h.agents.running() == 0).await;
    h.dm("telegram", "1", "and now?").await;
    said(&h.adapter("telegram"), "echo: and now?").await;
    let sessions = h.agents.turn_sessions();
    assert_eq!(sessions.len(), 2, "the queued follow-up was dropped");
    assert_eq!(sessions[0], sessions[1], "same session after /stop");
    h.dm("telegram", "1", "/stop").await;
    said(&h.adapter("telegram"), "No active task to stop.").await;
}
