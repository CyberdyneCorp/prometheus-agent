//! Routing (spec: messaging-gateway, Profiles served by one daemon;
//! Routing identity and deterministic session keys; Single-flight session
//! routing; Conversations never rotate on time).

use jiff::{SignedDuration, Timestamp};
use pmt_gateway::messaging::{Admission, ChatType, Inbound};
use pmt_store::gateway::routes::ResetReason;

use crate::support::{Harness, Setup, eventually, open, said};

#[tokio::test]
async fn an_event_for_an_unserved_profile_is_dropped() {
    let h = Harness::start(Setup::new(&open("relay"))).await;
    let mut inbound = Inbound::text(ChatType::Dm, "1", "1", "hello");
    inbound.profile = Some("ghost".into());
    assert_eq!(h.send("relay", inbound).await, Admission::Dropped);
    assert!(h.sessions().is_empty(), "no session is created");
    assert!(h.agents.texts().is_empty());
}

#[tokio::test]
async fn a_burst_of_first_messages_creates_one_session() {
    let mut config = open("telegram");
    config = config.replace("debounce_ms = 0", "debounce_ms = 50");
    let h = Harness::start(Setup::new(&config)).await;
    for n in 0..5 {
        let admission = h.dm("telegram", "123", &format!("part {n}")).await;
        assert_eq!(admission, Admission::Accepted);
    }
    eventually("the turn starts", || !h.agents.texts().is_empty()).await;
    said(&h.adapter("telegram"), "echo: part 0").await;
    assert_eq!(h.sessions().len(), 1, "exactly one session");
    let texts = h.agents.texts();
    assert_eq!(texts, vec!["part 0\npart 1\npart 2\npart 3\npart 4"]);
}

#[tokio::test]
async fn profiles_route_the_same_chat_to_their_own_sessions() {
    let setup = Setup {
        profiles: vec![("default", open("telegram")), ("main", open("telegram"))],
        ..Setup::new("")
    };
    let h = Harness::start(setup).await;
    h.dm("telegram", "123", "hi").await;
    h.send_as(
        "main",
        "telegram",
        Inbound::text(ChatType::Dm, "123", "123", "hi"),
    )
    .await;
    said(&h.adapter_of("main", "telegram"), "echo: hi").await;
    said(&h.adapter("telegram"), "echo: hi").await;
    let default = h.store().route("default:telegram:dm:123").unwrap().unwrap();
    let main = h
        .agents
        .store_of("main")
        .route("main:telegram:dm:123")
        .unwrap()
        .unwrap();
    assert_ne!(default.session_id, main.session_id);
    assert_eq!(h.sessions_of("main"), vec![main.session_id]);
}

#[tokio::test]
async fn group_members_get_sessions_of_their_own_and_a_thread_is_shared() {
    let h = Harness::start(Setup::new(&open("discord"))).await;
    for user in ["A", "B"] {
        h.send(
            "discord",
            Inbound::text(ChatType::Channel, "555", user, "hi"),
        )
        .await;
    }
    for user in ["A", "B"] {
        let mut reply = Inbound::text(ChatType::Thread, "C1", user, "in thread");
        reply.thread_id = Some("1700.1".into());
        h.send("discord", reply).await;
    }
    eventually("four turns", || h.agents.texts().len() == 4).await;
    let store = h.store();
    assert!(
        store
            .route("default:discord:channel:555:u:A")
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .route("default:discord:channel:555:u:B")
            .unwrap()
            .is_some()
    );
    let thread = store.route("default:discord:thread:C1:t:1700.1").unwrap();
    assert!(thread.is_some(), "one thread key without a participant");
    assert_eq!(h.sessions().len(), 3);
}

#[tokio::test]
async fn a_month_old_conversation_continues_its_session() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.dm("telegram", "123", "first").await;
    said(&h.adapter("telegram"), "echo: first").await;
    let key = "default:telegram:dm:123";
    let before = h.store().route(key).unwrap().unwrap().session_id;
    let month_ago = Timestamp::now() - SignedDuration::from_hours(24 * 30);
    h.store().touch_route(key, month_ago).unwrap();
    h.dm("telegram", "123", "again").await;
    said(&h.adapter("telegram"), "echo: again").await;
    assert_eq!(h.store().route(key).unwrap().unwrap().session_id, before);
    assert_eq!(h.sessions().len(), 1);
}

#[tokio::test]
async fn a_pruned_route_starts_a_new_session_marked_stale() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.dm("telegram", "123", "first").await;
    said(&h.adapter("telegram"), "echo: first").await;
    let key = "default:telegram:dm:123";
    let old = h.store().route(key).unwrap().unwrap().session_id;
    let long_ago = Timestamp::now() - SignedDuration::from_hours(24 * 120);
    h.store().touch_route(key, long_ago).unwrap();
    let cutoff = Timestamp::now() - SignedDuration::from_hours(24 * 90);
    assert_eq!(h.store().prune_routes(cutoff).unwrap(), 1);
    assert!(h.store().session(old).is_ok(), "the transcript stays");
    h.dm("telegram", "123", "back").await;
    said(&h.adapter("telegram"), "echo: back").await;
    let new = h.store().route(key).unwrap().unwrap().session_id;
    assert_ne!(new, old);
    let state = h.store().routed_session(new).unwrap().unwrap();
    assert_eq!(state.reset_reason, Some(ResetReason::StaleRoute));
    assert_eq!(h.store().session(new).unwrap().parent_session_id, Some(old));
}

#[tokio::test]
async fn pairing_is_stored_in_the_owning_profile() {
    let setup = Setup {
        profiles: vec![
            ("default", crate::support::config("telegram", "")),
            ("work", crate::support::config("telegram", "")),
        ],
        ..Setup::new("")
    };
    let h = Harness::start(setup).await;
    let admission = h
        .send_as(
            "work",
            "telegram",
            Inbound::text(ChatType::Dm, "77", "77", "hi"),
        )
        .await;
    assert_eq!(admission, Admission::PairingIssued);
    assert_eq!(h.agents.store_of("work").pairings(None).unwrap().len(), 1);
    assert!(
        h.agents
            .store_of("default")
            .pairings(None)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(start_paused = true)]
async fn an_evicted_agent_is_unloaded_in_its_own_profile() {
    let setup = Setup {
        profiles: vec![("default", open("telegram")), ("work", open("telegram"))],
        ..Setup::new("")
    };
    let h = Harness::start(setup).await;
    h.send_as(
        "work",
        "telegram",
        Inbound::text(ChatType::Dm, "9", "9", "hi"),
    )
    .await;
    said(&h.adapter_of("work", "telegram"), "echo: hi").await;
    let session = h.sessions_of("work")[0].to_string();
    tokio::time::advance(std::time::Duration::from_secs(3700)).await;
    eventually("the idle agent is unloaded", || {
        !h.agents.evicted.lock().unwrap().is_empty()
    })
    .await;
    let evicted = h.agents.evicted.lock().unwrap().clone();
    assert_eq!(evicted, vec![("work".to_owned(), session)]);
}
