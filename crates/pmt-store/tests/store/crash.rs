//! A turn's rows are persisted all-or-none (spec: session-storage,
//! Transactional message persistence), checked as properties: a batch with
//! a failing row stores nothing, and a writer process killed at a random
//! moment leaves only whole turns.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pmt_core::id::SessionId;
use pmt_store::{NewMessage, Role, Store, Visibility};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use serde_json::json;

use crate::support::{TestHome, session};

const PARTS: usize = 4;
const ROOT_VAR: &str = "PMT_STORE_CRASH_ROOT";
const SESSION_VAR: &str = "PMT_STORE_CRASH_SESSION";

/// One turn: user, assistant with a tool call, tool result, answer.
fn turn(n: u64) -> Vec<NewMessage> {
    let mut parts: Vec<NewMessage> = [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
        .into_iter()
        .enumerate()
        .map(|(k, role)| NewMessage::new(role, format!("turn {n} part {k}")))
        .collect();
    parts[1].tool_calls = Some(json!([{"id": format!("call-{n}"), "name": "terminal"}]));
    parts
}

fn runner(cases: u32) -> TestRunner {
    TestRunner::new(Config {
        cases,
        failure_persistence: None,
        ..Config::default()
    })
}

#[test]
fn failing_batch_stores_nothing() {
    let home = TestHome::new();
    let store = home.store("default");
    let chat = session(&store);
    let existing = NewMessage::new(Role::User, "anchor");
    store.append_message(chat.id, &existing).unwrap();
    let strategy = (1usize..8, proptest::option::of(0usize..8), any::<bool>());
    runner(64)
        .run(&strategy, |(len, failing, big)| {
            let before = store.session(chat.id).unwrap();
            let rows_before = store.messages(chat.id, Visibility::All).unwrap().len();
            let mut batch: Vec<NewMessage> = (0..len)
                .map(|i| {
                    let size = if big && i == 0 { 70 * 1024 } else { 16 };
                    NewMessage::new(Role::Assistant, "y".repeat(size))
                })
                .collect();
            let failing = failing.filter(|i| *i < len);
            if let Some(i) = failing {
                batch[i].id = existing.id;
            }
            let result = store.append_messages(chat.id, &batch);
            let rows_after = store.messages(chat.id, Visibility::All).unwrap().len();
            let after = store.session(chat.id).unwrap();
            if failing.is_some() {
                prop_assert!(result.is_err());
                prop_assert_eq!(rows_after, rows_before);
                prop_assert_eq!(after.message_count, before.message_count);
            } else {
                prop_assert!(result.is_ok());
                prop_assert_eq!(rows_after, rows_before + len);
                prop_assert_eq!(after.message_count, before.message_count + len as u64);
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn killed_writer_leaves_whole_turns_only() {
    let most = std::cell::Cell::new(0);
    runner(6)
        .run(&(0u64..60), |delay_ms| {
            most.set(most.get().max(check_crash(Duration::from_millis(delay_ms))));
            Ok(())
        })
        .unwrap();
    assert!(most.get() > 0, "the writer never committed a turn");
}

/// Kills a writer after `delay`; returns how many whole turns it stored.
fn check_crash(delay: Duration) -> usize {
    let home = TestHome::new();
    let chat = session(&home.store("default")).id;
    let root = home.home.root().to_path_buf();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "crash::crash_child_writer",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env(ROOT_VAR, &root)
        .env(SESSION_VAR, chat.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&root.join("writer-ready"));
    std::thread::sleep(delay);
    child.kill().unwrap();
    child.wait().unwrap();
    assert_whole_turns(&home.store("default"), chat)
}

fn wait_for(marker: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "writer never started");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn assert_whole_turns(store: &Store, chat: SessionId) -> usize {
    let rows = store.messages(chat, Visibility::All).unwrap();
    assert_eq!(rows.len() % PARTS, 0, "a partial turn was stored");
    for (n, turn_rows) in rows.chunks(PARTS).enumerate() {
        for (k, row) in turn_rows.iter().enumerate() {
            assert_eq!(
                row.content.as_deref(),
                Some(format!("turn {n} part {k}").as_str())
            );
        }
    }
    let record = store.session(chat).unwrap();
    assert_eq!(
        record.message_count,
        rows.len() as u64,
        "counters match rows"
    );
    assert_eq!(record.tool_call_count, (rows.len() / PARTS) as u64);
    rows.len() / PARTS
}

/// The writer process of `killed_writer_leaves_whole_turns_only`: appends
/// turns until it is killed.
#[test]
#[ignore = "child process of killed_writer_leaves_whole_turns_only"]
fn crash_child_writer() {
    let (Ok(root), Ok(chat)) = (std::env::var(ROOT_VAR), std::env::var(SESSION_VAR)) else {
        return;
    };
    let home = TestHome::at(Path::new(&root));
    let store = home.store("default");
    let chat = SessionId::parse(&chat).unwrap();
    let mut next = store.session(chat).unwrap().message_count / PARTS as u64;
    std::fs::write(Path::new(&root).join("writer-ready"), "").unwrap();
    loop {
        store.append_messages(chat, &turn(next)).unwrap();
        next += 1;
    }
}
