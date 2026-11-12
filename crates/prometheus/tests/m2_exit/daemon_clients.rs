//! Task 60.1: one daemon serves the TUI, a CLI protocol client and a
//! messaging platform at once (spec: workspace-architecture, Clients
//! attach to a running daemon; tui, TUI attaches to the gateway daemon as
//! a client; messaging-gateway, Event subscription and owned replay). A
//! message arriving through the mock platform adapter streams its reply
//! to the platform and, word by word, to the attached CLI client; the
//! client's connection drops, the session goes on, and the reconnected
//! client gets every event it missed, in order and without a gap, before
//! the next live one. A connection of another profile sees nothing.

use std::path::Path;
use std::time::Duration;

use pmt_rpc::client::{Client, Inbox};
use pmt_rpc::contracts::{Envelope, EventsSince};
use serde_json::json;

use crate::common::gateway::{Daemon, gateway_home_with, send, wait, wait_said};
use crate::common::tui::{enter, finish, opened, see, tui};
use crate::common::{daemon, ok, run};

const RULES: &str = r#"
[[rule]]
user = "from the tui"
repeat = true
text = "the tui got its answer"

[[rule]]
user = "platform one"
text = "first answer on the platform"

[[rule]]
user = "platform two"
text = "second answer streamed word by word"

[[rule]]
user = "platform three"
text = "third answer while the client was away"

[[rule]]
user = "platform four"
text = "fourth answer arrives live"
"#;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// The session the platform's chat is routed to.
fn routed_session(home: &Path) -> String {
    let db = rusqlite::Connection::open_with_flags(
        home.join("profiles/default/state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    db.query_row("SELECT session_id FROM gateway_sessions", [], |r| r.get(0))
        .unwrap()
}

/// The session's events until its next `turn.finished`, skipping other
/// sessions' events.
async fn turn_events(inbox: &mut Inbox, session: &str) -> Vec<Envelope> {
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_mins(1), inbox.events.recv())
            .await
            .expect("the turn finishes within a minute")
            .expect("the connection is open");
        if event.session_id.as_deref() != Some(session) {
            continue;
        }
        let done = event.event == "turn.finished";
        events.push(event);
        if done {
            return events;
        }
    }
}

/// The text of the `assistant.delta` events, joined.
fn streamed(events: &[Envelope]) -> (usize, String) {
    let deltas: Vec<&str> = events
        .iter()
        .filter(|e| e.event == "session.event" && e.data["type"] == "assistant.delta")
        .map(|e| e.data["payload"]["text"].as_str().unwrap_or_default())
        .collect();
    (deltas.len(), deltas.concat())
}

fn assert_gapless(events: &[Envelope], after: u64) {
    let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
    let expected: Vec<u64> = (after + 1..=after + seqs.len() as u64).collect();
    assert_eq!(
        seqs,
        expected,
        "every event once, in order, no gap: {:?}",
        events
            .iter()
            .map(|e| format!("{} {} {}", e.seq, e.event, e.data["type"]))
            .collect::<Vec<_>>()
    );
}

async fn attach(client: &Client, session: &str, last_seen: Option<u64>) {
    client
        .call(
            "session.attach",
            json!({"session_id": session, "role": "observer", "last_seen_seq": last_seen}),
        )
        .await
        .unwrap();
}

/// Scenarios: Reconnect resumes without gaps; Foreign profile session is
/// invisible (messaging-gateway, Event subscription and owned replay).
#[test]
fn the_daemon_serves_tui_cli_and_platform_and_a_dropped_client_resumes_without_gaps() {
    let home = gateway_home_with(RULES, "\n[gateway]\nmultiplex = true\n");
    ok(&run(
        home.path(),
        &["profile", "create", "work", "--no-alias"],
    ));
    for (key, value) in [("model.provider", "mock"), ("model.default", "mock")] {
        ok(&run(
            home.path(),
            &["-p", "work", "config", "set", key, value],
        ));
    }
    let daemon = Daemon::start(home.path());

    // The TUI attaches to the running daemon and runs a turn there.
    let mut tui = tui(home.path(), &[], &[]);
    opened(&mut tui);
    enter(&mut tui, "from the tui");
    see(&mut tui, "the tui got its answer");

    // A platform message opens the chat's session.
    send(home.path(), "default", "alice", "platform one");
    wait_said(home.path(), "default", "first answer on the platform");
    let session = routed_session(home.path());

    let rt = runtime();
    let last_seen = rt.block_on(async {
        let (client, mut inbox) = daemon::connect(home.path(), "default").await;
        attach(&client, &session, None).await;
        send(home.path(), "default", "alice", "platform two");
        let events = turn_events(&mut inbox, &session).await;
        let (deltas, text) = streamed(&events);
        assert!(deltas > 1, "the reply streams in parts: {events:?}");
        assert_eq!(text, "second answer streamed word by word");
        assert_gapless(&events, events[0].seq - 1);
        // The connection drops here, between turns.
        events.last().unwrap().seq
    });
    wait_said(
        home.path(),
        "default",
        "second answer streamed word by word",
    );

    // The session goes on while no client is attached.
    send(home.path(), "default", "alice", "platform three");
    wait_said(
        home.path(),
        "default",
        "third answer while the client was away",
    );

    rt.block_on(async {
        let (client, mut inbox) = daemon::connect(home.path(), "default").await;
        attach(&client, &session, None).await;
        let replay: EventsSince = serde_json::from_value(
            client
                .call(
                    "session.events.since",
                    json!({"session_id": session, "last_seen": last_seen}),
                )
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(!replay.truncated);
        assert_gapless(&replay.events, last_seen);
        assert_eq!(replay.latest_seq, replay.events.last().unwrap().seq);
        let missed: Vec<_> = replay
            .events
            .iter()
            .filter(|e| e.event == "session.event")
            .map(|e| e.data["type"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert!(missed.contains(&"user.message".to_owned()), "{missed:?}");
        assert_eq!(
            streamed(&replay.events).1,
            "third answer while the client was away"
        );
        // The next turn's live events follow the replayed ones directly.
        send(home.path(), "default", "alice", "platform four");
        let live = turn_events(&mut inbox, &session).await;
        assert_gapless(&live, replay.latest_seq);
        assert_eq!(streamed(&live).1, "fourth answer arrives live");

        // Another profile's connection is told nothing about the session.
        let (work, _inbox) = daemon::connect(home.path(), "work").await;
        let denied = work
            .call(
                "session.events.since",
                json!({"session_id": session, "last_seen": 0}),
            )
            .await
            .unwrap_err();
        assert_eq!(denied.code, -32_002);
        let denied = work
            .call("session.attach", json!({"session_id": session}))
            .await
            .unwrap_err();
        assert_eq!(denied.code, -32_002);
    });

    // The TUI stayed attached through all of it.
    enter(&mut tui, "from the tui");
    see(&mut tui, "the tui got its answer");
    enter(&mut tui, "/quit");
    assert_eq!(finish(tui).1, Some(0));
    wait(
        "the platform got four answers",
        Duration::from_secs(30),
        || crate::common::gateway::texts(home.path(), "default").len() >= 4,
    );
    assert_eq!(daemon.stop(home.path()), Some(0));
}
