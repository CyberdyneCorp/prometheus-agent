//! Scheduled deliveries beyond plain chats, inside the real daemon with
//! the debug-build file adapter (spec: scheduled-jobs, Bot Chat delivery
//! target; Continuable deliveries; Push notification flag, cron threads
//! and delivery confirmation).

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use self::common::gateway::{Daemon, gateway_home_with, send_in, sent, wait, wait_said};
use self::common::{ok, run};

const CRON: &str = "\n[cron]\ntick_interval_secs = 10\ntimezone = \"UTC\"\n";

fn json(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend(args);
    serde_json::from_str(&ok(&run(home, &all))).unwrap()
}

fn add(home: &Path, profile: &str, args: &[&str]) -> String {
    let mut all = vec!["-p", profile, "--json", "cron", "add"];
    all.extend(args);
    all.push("--yes");
    let doc: Value = serde_json::from_str(&ok(&run(home, &all))).unwrap();
    doc["job"]["id"].as_str().unwrap().to_owned()
}

fn store_of(home: &Path, profile: &str) -> pmt_store::Store {
    let layout = pmt_config::Home::open(home).unwrap();
    let dir = layout
        .existing_profile(&pmt_config::ProfileName::parse(profile).unwrap())
        .unwrap();
    let scope = pmt_config::ProfileScope::open(
        dir,
        &pmt_config::Overrides::default(),
        pmt_config::Launch::Other,
    )
    .unwrap();
    pmt_store::Store::open(&scope).unwrap()
}

/// Makes job `id` of `profile` due now.
fn due_now(home: &Path, profile: &str, id: &str) {
    let now = jiff::Timestamp::now().as_millisecond();
    store_of(home, profile)
        .cron_modify_job(id, |row| {
            row.next_run_at = Some(now - 1_000);
            Ok(())
        })
        .unwrap();
}

fn wait_status(home: &Path, profile: &str, id: &str, done: &[&str]) -> Value {
    let mut run = Value::Null;
    wait("the run settles", Duration::from_mins(2), || {
        run = json(home, &["-p", profile, "cron", "runs", id])["runs"][0].clone();
        run["status"].as_str().is_some_and(|s| done.contains(&s))
    });
    run
}

/// A served profile `name` answering with `rules`.
fn profile(home: &Path, name: &str, rules: &str, extra: &[(&str, &str)]) {
    ok(&run(home, &["profile", "create", name, "--no-alias"]));
    std::fs::write(home.join(format!("profiles/{name}/mock.toml")), rules).unwrap();
    let base = [
        ("model.provider", "mock"),
        ("model.default", "mock"),
        ("providers.mock.script", "mock.toml"),
    ];
    for (key, value) in base.iter().chain(extra) {
        ok(&run(home, &["-p", name, "config", "set", key, value]));
    }
}

/// The text of `session`'s messages of `role`.
fn said(store: &pmt_store::Store, session: pmt_core::id::SessionId, role: &str) -> Vec<String> {
    store
        .messages(session, pmt_store::Visibility::Active)
        .unwrap()
        .into_iter()
        .filter(|m| m.role.as_str() == role)
        .map(|m| m.content.unwrap_or_default())
        .collect()
}

/// The profile's canonical Bot Chat, once it has one.
fn bot_chat(store: &pmt_store::Store) -> Option<pmt_store::Session> {
    let all = pmt_store::ListOptions {
        all: true,
        ..pmt_store::ListOptions::default()
    };
    store
        .list_sessions(&all)
        .unwrap()
        .into_iter()
        .find(|s| s.bot_chat)
}

/// A job of the default profile named `name` delivering to `target`.
fn bot_chat_job(home: &Path, name: &str, target: &str) -> String {
    let deliver = format!("bot_chat:{target}");
    let args = [
        "every 1h",
        "--prompt",
        "find leads",
        "--name",
        name,
        "--deliver",
        &deliver,
    ];
    add(home, "default", &args)
}

/// Scenarios: Output processed by another profile's bot; Unknown profile
/// rejected at create.
#[test]
fn another_profiles_bot_chat_takes_one_turn_per_receipt() {
    let home = gateway_home_with(
        "[[rule]]\nrepeat = true\ntext = \"scout found three leads\"\n",
        &format!("{CRON}\n[gateway]\nmultiplex = true\n"),
    );
    profile(
        home.path(),
        "assistant",
        "[[rule]]\nuser = \"[Scheduled: leads]\"\nrepeat = true\ntext = \"assistant filed the leads\"\n",
        &[],
    );
    let refused = run(
        home.path(),
        &[
            "cron",
            "add",
            "every 1h",
            "--prompt",
            "x",
            "--deliver",
            "bot_chat:nobody",
            "--yes",
        ],
    );
    assert_ne!(refused.status.code(), Some(0));
    assert!(common::stderr(&refused).contains("unknown profile 'nobody'"));
    let job = bot_chat_job(home.path(), "leads", "assistant");
    due_now(home.path(), "default", &job);
    let daemon = Daemon::start(home.path());
    let done = ["ok", "delivery_failed", "failed"];
    let run_row = wait_status(home.path(), "default", &job, &done);
    assert_eq!(run_row["status"], "ok", "{run_row}");
    let receipt = run_row["receipt"]
        .as_str()
        .expect("the run shows its receipt");
    assert!(receipt.starts_with("dlv_"), "{receipt}");

    let assistant = store_of(home.path(), "assistant");
    let chat = bot_chat(&assistant).expect("assistant's bot chat was created");
    assert_eq!(chat.source, pmt_store::Source::Desktop);
    let mut replies = Vec::new();
    wait("the bot chat answers", Duration::from_mins(1), || {
        replies = said(&assistant, chat.id, "assistant");
        !replies.is_empty()
    });
    assert_eq!(replies, vec!["assistant filed the leads".to_owned()]);
    assert_eq!(
        said(&assistant, chat.id, "user"),
        vec!["[Scheduled: leads]\n\nscout found three leads".to_owned()]
    );

    // A retried delivery reuses its receipt: no second turn.
    let scout = store_of(home.path(), "default");
    scout
        .cron_settle_delivery(receipt, "pending", None, 0)
        .unwrap();
    wait("the retry is sent", Duration::from_mins(1), || {
        scout
            .cron_deliveries_of(run_row["run_id"].as_str().unwrap())
            .unwrap()
            .iter()
            .all(|d| d.state == "sent")
    });
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(said(&assistant, chat.id, "user").len(), 1);
    assert_eq!(said(&assistant, chat.id, "assistant").len(), 1);

    daemon.stop(home.path());
}

/// Bot Chat delivery target: a profile this daemon does not serve fails the
/// delivery with no fallback turn.
#[test]
fn an_unserved_profiles_bot_chat_fails_the_delivery() {
    let home = gateway_home_with(
        "[[rule]]\nrepeat = true\ntext = \"scout found three leads\"\n",
        &format!("{CRON}\n[gateway]\nmultiplex = true\n"),
    );
    profile(
        home.path(),
        "offline",
        "[[rule]]\nrepeat = true\ntext = \"never\"\n",
        &[("gateway.standalone", "true")],
    );
    let stray = bot_chat_job(home.path(), "stray", "offline");
    due_now(home.path(), "default", &stray);
    let daemon = Daemon::start(home.path());
    let done = ["ok", "delivery_failed", "failed"];
    let stray_run = wait_status(home.path(), "default", &stray, &done);
    assert_eq!(stray_run["status"], "delivery_failed", "{stray_run}");
    assert!(
        stray_run["error"]
            .as_str()
            .is_some_and(|e| e.contains("profile 'offline' is not served by this daemon")),
        "{stray_run}"
    );
    assert!(bot_chat(&store_of(home.path(), "offline")).is_none());
    daemon.stop(home.path());
}

/// Scenario: Reply to a daily brief continues it (the file adapter opens
/// threads as Telegram opens topics). Deliveries notify by default.
#[test]
fn a_reply_in_the_delivery_thread_continues_with_the_brief() {
    let home = gateway_home_with(
        "[[rule]]\nuser_regex = '(?s)Cron delivery: brief.*1\\. rotate keys.*expand point 2'\n\
         repeat = true\ntext = \"point 2 expanded with the brief\"\n\n\
         [[rule]]\nrepeat = true\ntext = \"1. rotate keys\\n2. renew certificates\"\n",
        CRON,
    );
    let job = add(
        home.path(),
        "default",
        &[
            "every 1h",
            "--prompt",
            "daily brief",
            "--name",
            "brief",
            "--deliver",
            "test:chat-5",
            "--attach-to-session",
            "true",
        ],
    );
    due_now(home.path(), "default", &job);
    let daemon = Daemon::start(home.path());
    let run_row = wait_status(
        home.path(),
        "default",
        &job,
        &["ok", "delivery_failed", "failed"],
    );
    assert_eq!(run_row["status"], "ok", "{run_row}");
    let messages = sent(home.path(), "default");
    let opened = messages
        .iter()
        .find(|m| m["kind"] == "thread")
        .expect("a thread was opened");
    assert_eq!(
        (&opened["chat"], &opened["title"]),
        (&json!("chat-5"), &json!("brief"))
    );
    let thread = opened["thread"].as_str().unwrap().to_owned();
    let delivery = messages
        .iter()
        .find(|m| m["kind"] == "send" && m["text"].as_str().is_some_and(|t| t.starts_with("⏰")))
        .expect("the brief was delivered");
    assert_eq!(delivery["thread"], json!(thread));
    assert_eq!(delivery["notify"], json!(true), "scheduled output notifies");

    send_in(
        home.path(),
        "default",
        "u1",
        "chat-5",
        &thread,
        "expand point 2",
    );
    wait_said(home.path(), "default", "point 2 expanded with the brief");
    let answer = sent(home.path(), "default")
        .into_iter()
        .find(|m| m["text"] == "point 2 expanded with the brief")
        .unwrap();
    assert_eq!(answer["thread"], json!(thread));
    daemon.stop(home.path());
}

/// Continuable deliveries, `in_channel` surface: on an adapter declaring
/// it, posted top-level and seeded into the chat's session.
#[test]
fn an_in_channel_delivery_seeds_the_chat_session() {
    use self::common::gateway::send;
    let home = gateway_home_with(
        "[[rule]]\nuser_regex = '(?s)Cron delivery: brief.*1\\. rotate keys.*expand point 2'\n\
         repeat = true\ntext = \"point 2 expanded in the channel\"\n\n\
         [[rule]]\nuser = \"hello\"\ntext = \"hi there\"\n\n\
         [[rule]]\nrepeat = true\ntext = \"1. rotate keys\\n2. renew certificates\"\n",
        "\n[cron]\ntick_interval_secs = 10\ntimezone = \"UTC\"\n\n\
         [cron.continuable_surface]\ntest = \"in_channel\"\n",
    );
    let daemon = Daemon::start(home.path());
    // The chat already talks to the agent: its session gets the seed.
    send(home.path(), "default", "chat-6", "hello");
    wait_said(home.path(), "default", "hi there");
    let job = add(
        home.path(),
        "default",
        &[
            "every 1h",
            "--prompt",
            "daily brief",
            "--name",
            "brief",
            "--deliver",
            "test:chat-6",
            "--attach-to-session",
            "true",
        ],
    );
    let store = store_of(home.path(), "default");
    due_now(home.path(), "default", &job);
    let run_row = wait_status(
        home.path(),
        "default",
        &job,
        &["ok", "delivery_failed", "failed"],
    );
    assert_eq!(run_row["status"], "ok", "{run_row}");
    let messages = sent(home.path(), "default");
    assert!(
        messages.iter().all(|m| m["kind"] != "thread"),
        "{messages:?}"
    );
    let session = store.chat_session("test", "chat-6", None).unwrap().unwrap();
    let seeded = said(&store, session, "user");
    assert!(
        seeded
            .iter()
            .any(|t| t.starts_with("[Cron delivery: brief]")),
        "{seeded:?}"
    );
    send(home.path(), "default", "chat-6", "expand point 2");
    wait_said(home.path(), "default", "point 2 expanded in the channel");
    daemon.stop(home.path());
}

/// `cron.delivery.notify = false` sends scheduled output silently.
#[test]
fn deliveries_are_silent_when_notify_is_off() {
    let home = gateway_home_with(
        "[[rule]]\nrepeat = true\ntext = \"all quiet\"\n",
        &format!("{CRON}\n[cron.delivery]\nnotify = false\n"),
    );
    let job = add(
        home.path(),
        "default",
        &["every 1h", "--prompt", "check", "--deliver", "test:chat-1"],
    );
    due_now(home.path(), "default", &job);
    let daemon = Daemon::start(home.path());
    let run_row = wait_status(
        home.path(),
        "default",
        &job,
        &["ok", "delivery_failed", "failed"],
    );
    assert_eq!(run_row["status"], "ok", "{run_row}");
    let delivery: Vec<Value> = sent(home.path(), "default")
        .into_iter()
        .filter(|m| m["kind"] == "send")
        .collect();
    assert_eq!(delivery.len(), 1, "{delivery:?}");
    assert_eq!(delivery[0]["notify"], json!(false));
    daemon.stop(home.path());
}
