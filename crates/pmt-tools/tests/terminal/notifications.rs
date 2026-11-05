//! What background processes tell their session (spec:
//! execution-environments, Quiet-wait auto-notify; Background
//! notifications are throttled; Subagent process handoff).

use std::sync::Arc;
use std::time::Duration;

use pmt_config::schema::TerminalConfig;
use pmt_tools::terminal::process::{Adopter, MAX_HANDOFFS, Notice, ProcessArgs, QUIET_NOTE};
use pmt_tools::terminal::{Terminal, TerminalArgs};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::common::{Fixture, ManualClock, exited};

fn notified(
    fixture: &Fixture,
    auto_notify: bool,
    clock: Option<&ManualClock>,
) -> (Terminal, mpsc::UnboundedReceiver<Notice>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let env = fixture.child_env(&[], &[]);
    let mut setup = fixture.setup(TerminalConfig::default(), env, fixture.sandbox(&[], None));
    setup.notify = Some(tx);
    setup.auto_notify = auto_notify;
    setup.clock = clock.map(ManualClock::clock);
    (Terminal::new(setup).unwrap(), rx)
}

async fn start(session: &Terminal, args: TerminalArgs) -> String {
    let args = TerminalArgs {
        background: true,
        ..args
    };
    let result = session.run(args).await;
    result
        .process_id
        .clone()
        .unwrap_or_else(|| panic!("not started: {result:?}"))
}

async fn act(session: &Terminal, action: &str, id: &str, data: Option<&str>) -> Value {
    session
        .processes()
        .handle(&ProcessArgs {
            action: action.to_owned(),
            process_id: Some(id.to_owned()),
            data: data.map(ToOwned::to_owned),
            timeout: Some(0.1),
        })
        .await
}

async fn next(rx: &mut mpsc::UnboundedReceiver<Notice>) -> Notice {
    tokio::time::timeout(Duration::from_secs(20), rx.recv())
        .await
        .expect("a notification")
        .expect("the channel is open")
}

/// Scenario: Second unchanged poll.
#[tokio::test]
async fn a_second_silent_poll_switches_on_completion_notification() {
    let fixture = Fixture::new();
    let (session, mut rx) = notified(&fixture, true, None);
    let id = start(&session, TerminalArgs::new("read line; echo got $line")).await;
    let first = act(&session, "poll", &id, None).await;
    assert_eq!(first["state"], "running");
    assert!(first.get("note").is_none(), "{first}");
    let second = act(&session, "wait", &id, None).await;
    assert_eq!(second["state"], "running");
    let note = second["note"].as_str().unwrap();
    assert!(note.ends_with(QUIET_NOTE), "{second}");
    // Any other action resets the count, but the switch stays on.
    act(&session, "submit", &id, Some("go")).await;
    let done = exited(next(&mut rx).await);
    assert_eq!((done.process_id.as_str(), done.exit_code), (id.as_str(), 0));
    session.close().await;
}

/// New output between two polls is not a quiet wait.
#[tokio::test]
async fn output_between_polls_keeps_plain_polling() {
    let fixture = Fixture::new();
    let (session, mut rx) = notified(&fixture, true, None);
    let id = start(&session, TerminalArgs::new("read a; echo one; read b")).await;
    act(&session, "poll", &id, None).await;
    act(&session, "submit", &id, Some("x")).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    act(&session, "poll", &id, None).await;
    let third = act(&session, "poll", &id, None).await;
    assert_eq!(
        third["note"], QUIET_NOTE,
        "two silent polls after the output"
    );
    act(&session, "kill", &id, None).await;
    assert!(rx.try_recv().is_err(), "a killed process is not announced");
    session.close().await;
}

/// Scenario: Cron job (the session's context turns the switch off).
#[tokio::test]
async fn without_auto_notify_polling_stays_plain_and_nothing_is_announced() {
    let fixture = Fixture::new();
    let (session, mut rx) = notified(&fixture, false, None);
    let id = start(&session, TerminalArgs::new("read line")).await;
    for _ in 0..3 {
        let polled = act(&session, "poll", &id, None).await;
        assert!(polled.get("note").is_none(), "{polled}");
    }
    act(&session, "close", &id, None).await;
    let waited = session
        .processes()
        .handle(&ProcessArgs {
            action: "wait".into(),
            process_id: Some(id.clone()),
            timeout: Some(10.0),
            ..ProcessArgs::default()
        })
        .await;
    assert_eq!(waited["state"], "exited");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(rx.try_recv().is_err(), "no completion was asked for");
    session.close().await;
}

/// Scenario: Ninth match.
#[tokio::test]
async fn the_ninth_match_disables_watching_and_falls_back_to_completion() {
    let fixture = Fixture::new();
    let clock = ManualClock::new();
    let (session, mut rx) = notified(&fixture, false, Some(&clock));
    let args = TerminalArgs {
        watch_patterns: vec!["hit".into()],
        ..TerminalArgs::new("while read l; do echo \"hit $l\"; done")
    };
    let id = start(&session, args).await;
    for n in 1..=9 {
        act(&session, "submit", &id, Some(&n.to_string())).await;
        if n <= 8 {
            match next(&mut rx).await {
                Notice::Matched(progress) => assert_eq!(progress.text, format!("hit {n}")),
                other => panic!("expected match {n}, got {other:?}"),
            }
        }
        clock.advance(Duration::from_secs(16));
    }
    // The ninth line arrives, but watching is off.
    for _ in 0..50 {
        if act(&session, "log", &id, None).await["output"]
            .as_str()
            .unwrap()
            .contains("hit 9")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(rx.try_recv().is_err(), "no ninth delivery");
    act(&session, "close", &id, None).await;
    let done = exited(next(&mut rx).await);
    assert_eq!(done.process_id, id);
    session.close().await;
}

/// A heartbeat comes at most once a minute and carries only new output.
#[tokio::test]
async fn heartbeats_are_clamped_to_a_minute_and_carry_new_output_only() {
    let fixture = Fixture::new();
    let (session, mut rx) = notified(&fixture, false, None);
    let args = TerminalArgs {
        heartbeat: Some(1.0),
        ..TerminalArgs::new("echo first; read line")
    };
    let started = tokio::time::Instant::now();
    let id = start(&session, args).await;
    for _ in 0..100 {
        let log = act(&session, "log", &id, None).await;
        if log["output"] == "first\n" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Virtual time from here on: the clamped beat is a minute away.
    tokio::time::pause();
    let beat = |notice: Notice| match notice {
        Notice::Heartbeat(progress) => progress,
        other => panic!("expected a heartbeat, got {other:?}"),
    };
    let wait = Duration::from_mins(10);
    let first = beat(
        tokio::time::timeout(wait, rx.recv())
            .await
            .unwrap()
            .unwrap(),
    );
    assert!(started.elapsed() >= Duration::from_mins(1));
    assert_eq!(
        (first.process_id.as_str(), first.text.as_str()),
        (id.as_str(), "first\n")
    );
    let second = beat(
        tokio::time::timeout(wait, rx.recv())
            .await
            .unwrap()
            .unwrap(),
    );
    assert_eq!(second.text, "", "nothing new since the last beat");
    tokio::time::resume();
    act(&session, "kill", &id, None).await;
    session.close().await;
}

/// Scenario: Fourth handoff.
#[tokio::test]
async fn a_subagent_hands_off_at_most_three_processes() {
    let fixture = Fixture::new();
    let (parent, mut parent_rx) = notified(&fixture, false, None);
    let parent = Arc::new(parent);
    let child = fixture.session();
    let lone = start(&child, TerminalArgs::new("sleep 30")).await;
    let refused = act(&child, "handoff", &lone, None).await;
    assert_eq!(
        refused["error"],
        "handoff is available only to subagent sessions"
    );
    child
        .processes()
        .set_adopter(Arc::clone(&parent) as Arc<dyn Adopter>);
    let quick = start(&child, TerminalArgs::new("read line")).await;
    let handed = act(&child, "handoff", &quick, None).await;
    assert_eq!(handed["handed_off"], true, "{handed}");
    let mut ids = vec![quick.clone()];
    for _ in 1..MAX_HANDOFFS {
        let id = start(&child, TerminalArgs::new("sleep 30")).await;
        assert_eq!(act(&child, "handoff", &id, None).await["status"], "ok");
        ids.push(id);
    }
    let fourth = act(&child, "handoff", &lone, None).await;
    assert_eq!(
        fourth["error"],
        "handoff cap reached (3 per subagent); wait on or kill the rest yourself."
    );
    assert_eq!(act(&child, "poll", &lone, None).await["state"], "running");
    // The parent owns the handed-off processes now, and hears of their end.
    for id in &ids {
        assert_eq!(act(&parent, "poll", id, None).await["state"], "running");
        assert!(act(&child, "poll", id, None).await["error"].is_string());
    }
    child.close().await;
    act(&parent, "submit", &quick, Some("bye")).await;
    let done = exited(next(&mut parent_rx).await);
    assert_eq!(done.process_id, quick);
    parent.close().await;
}
