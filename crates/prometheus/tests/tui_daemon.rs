//! The daemon behind the TUI, through its client protocol (spec: tui,
//! Prompt submission while busy; Turn authors are stamped by the gateway;
//! Discovery RPCs preserve the prompt cache; Composer and path
//! completion): busy inputs are queued, steered or run after an
//! interrupt, never dropped; each stored input names its author; the
//! discovery methods leave the session's system prompt alone; path
//! completion lists the session's own working directory.

mod common;

use std::path::Path;
use std::time::Duration;

use common::{daemon, mock_home};
use pmt_rpc::client::{Client, Inbox};
use pmt_rpc::contracts::Envelope;
use serde_json::{Value, json};

/// A script whose `slow` prompt answers after a few seconds and every other one at
/// once.
fn script(home: &Path) -> String {
    let path = home.join("busy.toml");
    std::fs::write(
        &path,
        "[[rule]]\nuser = \"slow\"\nrepeat = true\nlatency_ms = 1000\ntext = \"slow done\"\n\n\
         [[rule]]\nrepeat = true\ntext = \"fast done\"\n",
    )
    .unwrap();
    path.to_string_lossy().into_owned()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

async fn session(client: &Client, script: &str) -> String {
    let created = client
        .call("session.create", json!({"setup": {"mock_script": script}}))
        .await
        .unwrap();
    created["session_id"].as_str().unwrap().to_owned()
}

async fn submit(client: &Client, session: &str, text: &str) -> Value {
    client
        .call(
            "prompt.submit",
            json!({"session_id": session, "text": text}),
        )
        .await
        .unwrap()
}

/// The next `turn.finished`.
async fn finished(inbox: &mut Inbox) -> Envelope {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), inbox.events.recv())
            .await
            .expect("a turn finishes within 30 s")
            .expect("the connection is open");
        if event.event == "turn.finished" {
            return event;
        }
    }
}

#[test]
fn an_input_sent_while_busy_is_queued_and_runs_after_the_turn() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let script = script(home.path());
    runtime().block_on(async {
        let (client, mut inbox) = daemon::connect(home.path(), "default").await;
        let session = session(&client, &script).await;
        let first = submit(&client, &session, "slow").await;
        assert_eq!(first["admission"], "started");
        let second = submit(&client, &session, "second").await;
        assert_eq!(second["admission"], "queued", "never dropped");
        let one = finished(&mut inbox).await;
        assert_eq!(one.data["turn_id"], first["turn_id"]);
        assert_eq!(one.data["final_response"], "slow done");
        let two = finished(&mut inbox).await;
        assert_eq!(two.data["turn_id"], second["turn_id"]);
        assert_eq!(two.data["final_response"], "fast done");
    });
}

#[test]
fn interrupt_mode_stops_the_turn_then_runs_the_input() {
    let home = mock_home();
    common::append_config(home.path(), "\n[busy]\ninput_mode = \"interrupt\"\n");
    let _daemon = daemon::start(home.path(), &[]);
    let script = script(home.path());
    runtime().block_on(async {
        let (client, mut inbox) = daemon::connect(home.path(), "default").await;
        let session = session(&client, &script).await;
        submit(&client, &session, "slow").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let started = std::time::Instant::now();
        let next = submit(&client, &session, "now").await;
        assert_eq!(next["admission"], "queued");
        let stopped = finished(&mut inbox).await;
        assert_eq!(stopped.data["exit_reason"], "interrupted_by_user");
        let ran = finished(&mut inbox).await;
        assert_eq!(ran.data["final_response"], "fast done");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "did not wait for the slow turn"
        );
    });
}

#[test]
fn steer_mode_hands_the_input_to_the_running_turn() {
    let home = mock_home();
    common::append_config(home.path(), "\n[busy]\ninput_mode = \"steer\"\n");
    let _daemon = daemon::start(home.path(), &[]);
    let script = script(home.path());
    runtime().block_on(async {
        let (client, mut inbox) = daemon::connect(home.path(), "default").await;
        let session = session(&client, &script).await;
        let running = submit(&client, &session, "slow").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let steered = submit(&client, &session, "also check the tests").await;
        assert_eq!(steered["admission"], "steered");
        assert_eq!(
            steered["turn_id"], running["turn_id"],
            "it joined the running turn"
        );
        let done = finished(&mut inbox).await;
        assert_eq!(done.data["turn_id"], running["turn_id"]);
    });
}

#[test]
fn each_stored_input_names_its_gateway_stamped_author() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let script = script(home.path());
    let session_id = runtime().block_on(async {
        let (client, mut inbox) = daemon::connect(home.path(), "default").await;
        let session = session(&client, &script).await;
        submit(&client, &session, "hello").await;
        finished(&mut inbox).await;
        session
    });
    let messages = common::session_messages(home.path(), &session_id);
    let user = messages.iter().find(|m| m["role"] == "user").unwrap();
    let author = &user["metadata"]["author"];
    assert_eq!(author["client_kind"], "cli", "{user}");
    assert!(author["client_id"].as_str().unwrap().starts_with("cli-"));
    assert_eq!(author["principal"], "default");
}

fn prompt_hash(home: &Path, session: &str) -> Option<String> {
    let db = rusqlite::Connection::open_with_flags(
        home.join("profiles/default/state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    db.query_row(
        "SELECT system_prompt_hash FROM sessions WHERE id = ?1",
        [session],
        |row| row.get(0),
    )
    .unwrap()
}

/// Scenario: Listing commands mid-conversation.
#[test]
fn discovery_calls_leave_the_cached_system_prompt_unchanged() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let script = script(home.path());
    runtime().block_on(async {
        let (client, mut inbox) = daemon::connect(home.path(), "default").await;
        let session = session(&client, &script).await;
        submit(&client, &session, "first").await;
        finished(&mut inbox).await;
        let before = prompt_hash(home.path(), &session).expect("the first turn set it");
        let id = json!({"session_id": session});
        let catalog = client.call("commands.catalog", id.clone()).await.unwrap();
        let names: Vec<&str> = catalog["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert!(
            names.contains(&"compress") && names.contains(&"model"),
            "{names:?}"
        );
        for (method, params) in [
            (
                "complete.slash",
                json!({"session_id": session, "prefix": "/mo"}),
            ),
            (
                "complete.path",
                json!({"session_id": session, "prefix": "./"}),
            ),
            ("model.options", id.clone()),
            ("session.transcript", id.clone()),
        ] {
            client.call(method, params).await.unwrap();
        }
        submit(&client, &session, "second").await;
        finished(&mut inbox).await;
        assert_eq!(
            prompt_hash(home.path(), &session).as_deref(),
            Some(before.as_str()),
            "the system prompt was not rebuilt"
        );
    });
}

#[test]
fn path_completion_lists_the_sessions_working_directory() {
    let home = mock_home();
    let work = home.path().join("work");
    std::fs::create_dir_all(work.join("src/lib")).unwrap();
    std::fs::write(work.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(work.join("src/.hidden"), "").unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    runtime().block_on(async {
        let (client, _inbox) = daemon::connect(home.path(), "default").await;
        let created = client
            .call("session.create", json!({"cwd": work.to_string_lossy()}))
            .await
            .unwrap();
        let session = created["session_id"].as_str().unwrap();
        client
            .call("session.resume", json!({"session_id": session}))
            .await
            .unwrap();
        let listed = client
            .call(
                "complete.path",
                json!({"session_id": session, "prefix": "./src/"}),
            )
            .await
            .unwrap();
        let texts: Vec<(&str, bool)> = listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| (i["text"].as_str().unwrap(), i["dir"].as_bool().unwrap()))
            .collect();
        assert_eq!(texts, [("./src/lib/", true), ("./src/main.rs", false)]);
    });
}

/// Scenario (scheduled-jobs, /cron slash command) on a client surface:
/// `/cron add` without `--yes` shows the echo; the client's next input
/// `yes` saves the job without starting a turn (`handled`, with the
/// outcome as its notice).
#[test]
fn a_yes_after_a_cron_echo_saves_the_job_without_a_turn() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let script = script(home.path());
    runtime().block_on(async {
        let (client, _inbox) = daemon::connect(home.path(), "default").await;
        let session = session(&client, &script).await;
        let shown = client
            .call(
                "command.dispatch",
                json!({"session_id": session, "name": "cron",
                    "args": "add \"every day at 18:00\" \"summarize today's commits\""}),
            )
            .await
            .unwrap();
        assert!(
            shown.to_string().contains("reply `yes` to save this job"),
            "{shown}"
        );
        let saved = submit(&client, &session, "yes").await;
        assert_eq!(saved["admission"], "handled", "{saved}");
        assert!(
            saved["notice"]
                .as_str()
                .is_some_and(|n| n.contains("created job")),
            "{saved}"
        );
    });
    let listed = common::ok(&common::run(home.path(), &["--json", "cron", "list"]));
    assert!(listed.contains("summarize today's commits"), "{listed}");
}
