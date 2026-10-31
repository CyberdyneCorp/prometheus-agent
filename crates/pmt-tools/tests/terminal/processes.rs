//! The background process registry (spec: execution-environments,
//! Background process registry).

use std::time::Duration;

use pmt_tools::terminal::process::{MAX_PROCESSES, ProcessArgs};
use pmt_tools::terminal::{LocalTerminal, TerminalArgs};
use serde_json::Value;

use crate::common::{Fixture, ManualClock, with_notifications};
use crate::foreground::gone;

async fn background(session: &LocalTerminal, command: &str) -> String {
    let args = TerminalArgs {
        background: true,
        ..TerminalArgs::new(command)
    };
    let result = session.run(args).await;
    assert!(result.process_id.is_some(), "not started: {result:?}");
    result.process_id.unwrap()
}

async fn act(session: &LocalTerminal, action: &str, id: Option<&str>) -> Value {
    session
        .processes()
        .handle(&ProcessArgs {
            action: action.to_owned(),
            process_id: id.map(ToOwned::to_owned),
            data: None,
            timeout: Some(30.0),
        })
        .await
}

async fn pid_from(path: &std::path::Path) -> i32 {
    for _ in 0..100 {
        if let Some(pid) = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{} was never written", path.display());
}

#[tokio::test]
async fn list_poll_wait_and_kill() {
    let fixture = Fixture::new();
    let session = fixture.session();
    let quick = background(&session, "echo one; echo two").await;
    let waited = act(&session, "wait", Some(&quick)).await;
    assert_eq!(waited["state"], "exited");
    assert_eq!(waited["exit_code"], 0);
    assert_eq!(waited["output"], "one\ntwo\n");
    // Poll returns only what is new since the last poll.
    assert_eq!(act(&session, "poll", Some(&quick)).await["output"], "");
    assert_eq!(
        act(&session, "log", Some(&quick)).await["output"],
        "one\ntwo\n"
    );

    let slow = background(&session, "sleep 300").await;
    let listed = act(&session, "list", None).await;
    assert_eq!(listed["processes"].as_array().unwrap().len(), 2);
    // A unique prefix of the id is enough.
    let prefix = &slow[..slow.len() - 10];
    let killed = act(&session, "kill", Some(prefix)).await;
    assert_eq!(killed["state"], "killed", "{killed}");
}

#[tokio::test]
async fn bad_requests_name_the_problem() {
    let fixture = Fixture::new();
    let session = fixture.session();
    let unknown = act(&session, "restart", None).await;
    assert_eq!(
        unknown["error"],
        "unknown process action: restart; use one of list, poll, log, wait, kill, write, submit, close, handoff"
    );
    assert_eq!(
        act(&session, "poll", None).await["error"],
        "process_id is required for poll"
    );
    assert!(
        act(&session, "kill", Some("proc_nope")).await["error"]
            .as_str()
            .unwrap()
            .starts_with("no process matches")
    );
}

#[tokio::test]
async fn stdin_can_be_written_and_closed() {
    let fixture = Fixture::new();
    let session = fixture.session();
    let id = background(
        &session,
        "while read line; do echo got:$line; done; echo eof",
    )
    .await;
    let submit = ProcessArgs {
        action: "submit".to_owned(),
        process_id: Some(id.clone()),
        data: Some("hello".to_owned()),
        timeout: None,
    };
    assert_eq!(session.processes().handle(&submit).await["status"], "ok");
    act(&session, "close", Some(&id)).await;
    let done = act(&session, "wait", Some(&id)).await;
    assert_eq!(done["output"], "got:hello\neof\n");
}

#[tokio::test]
async fn no_orphan_remains_after_the_session_closes() {
    let fixture = Fixture::new();
    let session = fixture.session();
    let pid_file = fixture.work.join("grandchild.pid");
    background(
        &session,
        &format!("sleep 300 & echo $! > {}; wait", pid_file.display()),
    )
    .await;
    let grandchild = pid_from(&pid_file).await;
    let leader = act(&session, "list", None).await["processes"][0]["pid"]
        .as_i64()
        .unwrap();
    session.close().await;
    for pid in [grandchild, i32::try_from(leader).unwrap()] {
        assert!(gone(pid).await, "process {pid} survived the session");
    }
}

#[tokio::test]
async fn dropping_the_session_also_reaps() {
    let fixture = Fixture::new();
    let session = fixture.session();
    let pid_file = fixture.work.join("grandchild.pid");
    background(
        &session,
        &format!("sleep 300 & echo $! > {}; wait", pid_file.display()),
    )
    .await;
    let grandchild = pid_from(&pid_file).await;
    drop(session);
    assert!(
        gone(grandchild).await,
        "process {grandchild} survived the dropped session"
    );
}

#[tokio::test]
async fn a_long_job_is_kept_for_thirty_minutes_after_it_finishes() {
    let fixture = Fixture::new();
    let time = ManualClock::new();
    let (session, _done) = with_notifications(&fixture, Some(time.clock()));
    let id = background(&session, "read line; echo final output; exit 3").await;
    // It runs for two hours, then finishes.
    time.advance(Duration::from_hours(2));
    act(&session, "close", Some(&id)).await;
    assert_eq!(act(&session, "wait", Some(&id)).await["exit_code"], 3);
    time.advance(Duration::from_mins(10));
    let polled = act(&session, "log", Some(&id)).await;
    assert_eq!(
        (polled["exit_code"].as_i64(), polled["output"].as_str()),
        (Some(3), Some("final output\n"))
    );
    time.advance(Duration::from_mins(20));
    assert!(
        act(&session, "poll", Some(&id)).await["error"]
            .as_str()
            .unwrap()
            .starts_with("no process matches")
    );
}

#[tokio::test]
async fn the_oldest_finished_process_makes_room_for_a_new_one() {
    let fixture = Fixture::new();
    let session = fixture.session();
    let mut ids = Vec::new();
    for _ in 0..MAX_PROCESSES {
        let id = background(&session, "true").await;
        act(&session, "wait", Some(&id)).await;
        ids.push(id);
    }
    background(&session, "true").await;
    let listed = act(&session, "list", None).await;
    let kept: Vec<&str> = listed["processes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["process_id"].as_str().unwrap())
        .collect();
    assert_eq!(kept.len(), MAX_PROCESSES);
    assert!(
        !kept.contains(&ids[0].as_str()),
        "the oldest finished one is pruned"
    );
}
