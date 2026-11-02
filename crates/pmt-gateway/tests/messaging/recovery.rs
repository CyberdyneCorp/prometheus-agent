//! Restarts (spec: messaging-gateway, Crash recovery and clean shutdown
//! marker; Stuck-loop suspension; Durable delivery outbox; All adapters
//! down policy). A crash is the gateway's runtime dropped mid-work: every
//! task dies at once, as when the process is killed.

use std::time::Duration;

use pmt_gateway::messaging::{
    AdapterError, AdapterState, CLEAN_SHUTDOWN, ChatType, Inbound, RESTARTED,
};
use pmt_store::gateway::outbox::{NewOutbound, OutboundKind, OutboundState};
use pmt_store::gateway::routes::ResetReason;

use crate::support::{Harness, Setup, eventually, open, said};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Runs `phase` on a fresh runtime, then drops it (a crash, unless the
/// phase stopped the gateway itself).
fn phase<T>(phase: impl std::future::Future<Output = T>) -> T {
    let rt = runtime();
    let out = rt.block_on(phase);
    drop(rt);
    out
}

#[test]
fn a_crash_mid_turn_is_recovered_on_the_next_start() {
    let home = phase(async {
        let h = Harness::start(Setup::new(&open("telegram"))).await;
        h.dm("telegram", "1", "slow:60000").await;
        eventually("the turn runs", || h.agents.running() == 1).await;
        h.home
    });
    assert!(!home.path().join("run").join(CLEAN_SHUTDOWN).exists());
    phase(async {
        let h = Harness::start_in(home, Setup::new(&open("telegram"))).await;
        said(&h.adapter("telegram"), RESTARTED).await;
        let session = h.sessions()[0];
        let state = h.store().routed_session(session).unwrap().unwrap();
        assert!(state.resume_pending);
        assert!(!state.turn_active);
        assert_eq!(state.restart_count, 1);
        h.dm("telegram", "1", "/retry").await;
        said(&h.adapter("telegram"), "Reply /retry").await;
        eventually("the retried turn runs", || {
            h.agents.texts() == vec!["the last message again"]
        })
        .await;
        said(&h.adapter("telegram"), "echo: the last message again").await;
        let state = h.store().routed_session(session).unwrap().unwrap();
        assert!(!state.resume_pending, "a completed turn clears it");
        let notices = h
            .adapter("telegram")
            .texts()
            .iter()
            .filter(|t| *t == RESTARTED)
            .count();
        assert_eq!(notices, 1, "told once");
    });
}

#[test]
fn a_clean_stop_skips_recovery_and_a_timed_out_drain_does_not() {
    let home = phase(async {
        let h = Harness::start(Setup::new(&open("telegram"))).await;
        h.dm("telegram", "1", "hi").await;
        said(&h.adapter("telegram"), "echo: hi").await;
        h.stop().await
    });
    assert!(home.path().join("run").join(CLEAN_SHUTDOWN).exists());
    let home = phase(async {
        let setup = Setup {
            drain: Duration::from_millis(300),
            ..Setup::new(&open("telegram"))
        };
        let h = Harness::start_in(home, setup).await;
        assert!(
            !h.home.path().join("run").join(CLEAN_SHUTDOWN).exists(),
            "the marker is consumed at start"
        );
        h.dm("telegram", "1", "slow:60000").await;
        eventually("the turn runs", || h.agents.running() == 1).await;
        h.stop().await
    });
    assert!(
        !home.path().join("run").join(CLEAN_SHUTDOWN).exists(),
        "a drain that timed out leaves no marker"
    );
    phase(async {
        let h = Harness::start_in(home, Setup::new(&open("telegram"))).await;
        said(&h.adapter("telegram"), RESTARTED).await;
    });
}

#[test]
fn the_third_restart_during_a_turn_suspends_the_session() {
    let mut home = phase(async {
        let h = Harness::start(Setup::new(&open("telegram"))).await;
        h.dm("telegram", "1", "slow:60000").await;
        eventually("the turn runs", || h.agents.running() == 1).await;
        h.home
    });
    for _ in 0..2 {
        home = phase(async {
            let h = Harness::start_in(home, Setup::new(&open("telegram"))).await;
            h.dm("telegram", "1", "slow:60000").await;
            eventually("the turn runs again", || h.agents.running() == 1).await;
            h.home
        });
    }
    phase(async {
        let h = Harness::start_in(home, Setup::new(&open("telegram"))).await;
        let old = h
            .store()
            .route("default:telegram:dm:1")
            .unwrap()
            .unwrap()
            .session_id;
        assert!(h.store().routed_session(old).unwrap().unwrap().suspended);
        h.dm("telegram", "1", "hello again").await;
        said(&h.adapter("telegram"), "echo: hello again").await;
        let new = h
            .store()
            .route("default:telegram:dm:1")
            .unwrap()
            .unwrap()
            .session_id;
        assert_ne!(new, old);
        let state = h.store().routed_session(new).unwrap().unwrap();
        assert_eq!(state.reset_reason, Some(ResetReason::Suspended));
        said(&h.adapter("telegram"), "this is a fresh session").await;
    });
}

#[test]
fn a_reply_stored_before_a_crash_is_sent_exactly_once() {
    let home = phase(async {
        let h = Harness::start(Setup::new(&open("telegram"))).await;
        // The platform is down: the reply stays queued in the outbox.
        h.adapter("telegram")
            .failures
            .lock()
            .unwrap()
            .extend((0..50).map(|_| pmt_gateway::messaging::SendError::Network("down".into())));
        let reply = NewOutbound {
            idempotency_key: "ses:turn:0".into(),
            session_id: None,
            platform: "telegram".into(),
            chat_id: "1".into(),
            thread_id: None,
            reply_to: None,
            edit_of: None,
            kind: OutboundKind::Reply,
            body: "the answer".into(),
        };
        h.store().enqueue_outbound(&reply).unwrap();
        h.home
    });
    phase(async {
        let h = Harness::start_in(home, Setup::new(&open("telegram"))).await;
        said(&h.adapter("telegram"), "the answer").await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let copies = h
            .adapter("telegram")
            .texts()
            .iter()
            .filter(|t| *t == "the answer")
            .count();
        assert_eq!(copies, 1);
        let sent = h
            .store()
            .outbox_entries(Some(OutboundState::Sent), 10)
            .unwrap();
        assert_eq!(sent.len(), 1);
    });
}

fn revoked() -> AdapterError {
    AdapterError {
        retryable: false,
        code: "unauthorized".into(),
        message: "the bot token was revoked".into(),
    }
}

#[tokio::test]
async fn a_revoked_token_keeps_local_clients_alive_by_default() {
    let setup = Setup {
        connect_errors: vec![(("default".into(), "telegram".into()), revoked())],
        ..Setup::new(&open("telegram"))
    };
    let h = Harness::start(setup).await;
    eventually("the adapter is fatal", || {
        h.gateway
            .statuses()
            .iter()
            .any(|s| s.state == AdapterState::Fatal)
    })
    .await;
    let status = h.server.status();
    assert_eq!(status.adapters[0].state, "fatal");
    assert_eq!(status.adapters[0].code.as_deref(), Some("unauthorized"));
    assert!(!h.server.retiring(), "the daemon keeps serving");
}

#[tokio::test]
async fn with_exit_the_daemon_stops_when_every_adapter_is_down() {
    let setup = Setup {
        connect_errors: vec![(("default".into(), "telegram".into()), revoked())],
        exit_when_down: true,
        ..Setup::new(&open("telegram"))
    };
    let h = Harness::start(setup).await;
    eventually("the daemon stops", || h.server.adapters_down()).await;
    assert!(h.server.retiring());
}

#[tokio::test]
async fn a_connected_adapter_is_degraded_until_a_send_succeeds() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    let status = &h.gateway.statuses()[0];
    assert_eq!(status.state, AdapterState::Degraded);
    assert_eq!(
        status.message.as_deref(),
        Some("connected but not yet confirmed active; recovering in background")
    );
    h.send("telegram", Inbound::text(ChatType::Dm, "1", "1", "hi"))
        .await;
    said(&h.adapter("telegram"), "echo: hi").await;
    assert_eq!(h.gateway.statuses()[0].state, AdapterState::Connected);
}
