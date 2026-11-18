//! Bots as profiles (spec: bot-mode, A bot is a profile with backend
//! metadata; Canonical Bot Chat; Creating, duplicating and deleting bots;
//! Avatars; Roster presentation, hiding and sections; Bot voices; Bot
//! routines; Bot Mode off switch and CLI parity), over a real daemon and
//! its desktop-style clients.

#![cfg(feature = "gateway")]

mod common;

use std::time::Duration;

use serde_json::{Value, json};

use self::common::bots::{self, bot, bot_chat, call, client, said, store, wait, wait_said};
use self::common::{daemon, ok, run, run_with_stdin};

fn row<'a>(roster: &'a Value, name: &str) -> Option<&'a Value> {
    roster["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["agent"]["name"] == name)
}

/// Scenario: Title follows the bot to a second desktop. Scenario:
/// Deterministic default face. Hidden bots, sections, previews and
/// unread counts come from the backend too.
#[test]
fn a_bots_metadata_is_the_same_for_every_client() {
    let home = bots::home("[[rule]]\nrepeat = true\ntext = \"hello there\"\n");
    bot(
        home.path(),
        "research",
        "[[rule]]\nrepeat = true\ntext = \"on it\"\n",
        &[],
    );
    bot(home.path(), "scribe", "", &[]);
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let all = ["default", "research", "scribe"];
    runtime.block_on(async {
        let a = client(home.path(), &all).await;
        let b = client(home.path(), &all).await;
        let updated = call(
            &a,
            "bot.meta.update",
            json!({"profile": "research", "changes": {
                "title": "Research Buddy",
                "section": {"id": "work", "name": "Work"},
                "avatar": {"kind": "geometric", "shape": "star", "color": "teal"}
            }}),
        )
        .await;
        assert_eq!(updated["title"], "Research Buddy");
        let listed = call(&b, "bot.roster.list", json!({"profile": "default"})).await;
        let research = row(&listed, "research").unwrap();
        assert_eq!(research["agent"]["title"], "Research Buddy", "{listed}");
        assert_eq!(research["chat"]["section"]["name"], "Work");
        assert_eq!(research["agent"]["avatar"]["shape"], "star");
        // Both clients get the same resolved face for a bot without one.
        let other = call(&a, "bot.roster.list", json!({"profile": "scribe"})).await;
        assert_eq!(
            row(&listed, "scribe").unwrap()["agent"]["avatar"],
            row(&other, "scribe").unwrap()["agent"]["avatar"]
        );
        assert_eq!(
            row(&other, "scribe").unwrap()["agent"]["avatar"]["kind"],
            "blob"
        );
        // An unknown avatar kind is refused and changes nothing.
        let refused = a
            .call(
                "bot.meta.update",
                json!({"profile": "scribe", "changes": {"avatar": {"kind": "hologram"}}}),
            )
            .await;
        assert!(refused.is_err(), "{refused:?}");
        // A hidden bot leaves the default roster only.
        call(
            &a,
            "bot.meta.update",
            json!({"profile": "scribe", "changes": {"hidden": true}}),
        )
        .await;
        let listed = call(&b, "bot.roster.list", json!({"profile": "default"})).await;
        assert!(row(&listed, "scribe").is_none(), "{listed}");
        let with_hidden = call(
            &b,
            "bot.roster.list",
            json!({"profile": "default", "include_hidden": true}),
        )
        .await;
        assert_eq!(row(&with_hidden, "scribe").unwrap()["chat"]["hidden"], true);
        // Deleting the section moves its bots to unassigned.
        let moved = call(
            &a,
            "bot.sections.delete",
            json!({"profile": "default", "section_id": "work"}),
        )
        .await;
        assert_eq!(moved["moved"], json!(["research"]));
        let meta = call(&b, "bot.meta.get", json!({"profile": "research"})).await;
        assert_eq!(meta["section"], Value::Null);
        assert_eq!(meta["title"], "Research Buddy", "the bot itself stays");
        // A reply in the Bot Chat is the roster's preview, unread until read.
        bots::tell(&a, "research", "status?").await;
    });
    wait_said(home.path(), "research", "assistant", "on it");
    runtime.block_on(async {
        let b = client(home.path(), &all).await;
        let listed = call(&b, "bot.roster.list", json!({"profile": "default"})).await;
        let chat = &row(&listed, "research").unwrap()["chat"];
        assert_eq!(chat["preview"], "on it", "{listed}");
        assert_eq!(chat["unread"], 1);
        assert_eq!(chat["active"], true);
        call(&b, "bot.chat.read", json!({"profile": "research"})).await;
        let listed = call(&b, "bot.roster.list", json!({"profile": "default"})).await;
        assert_eq!(row(&listed, "research").unwrap()["chat"]["unread"], 0);
    });
}

/// Scenario: OAuth not copied. A new bot gets its Bot Chat and its
/// introduction; a duplicate copies the metadata; `default` is never
/// deleted.
#[test]
fn creating_duplicating_and_deleting_bots() {
    let home = bots::home("[[rule]]\nrepeat = true\ntext = \"Hi, I am your new bot.\"\n");
    for (name, value) in [
        ("OPENROUTER_API_KEY", "sk-or-test-0123456789"),
        ("OAUTH_ANTHROPIC_DEFAULT_ACCESS", "oauth-access-token"),
        ("OAUTH_ANTHROPIC_DEFAULT_META", "{}"),
    ] {
        ok(&run_with_stdin(
            home.path(),
            &["secrets", "set", name],
            &format!("{value}\n"),
        ));
    }
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut introduction = String::new();
    runtime.block_on(async {
        let c = client(home.path(), &["default"]).await;
        let created = call(
            &c,
            "bot.create",
            json!({"profile": "default", "name": "scout", "title": "Scout", "copy_api_keys": true}),
        )
        .await;
        assert_eq!(created["profile"], "scout");
        assert_eq!(
            created["copied_secrets"],
            json!(["OPENROUTER_API_KEY"]),
            "{created}"
        );
        assert!(
            created["introduction"]
                .as_str()
                .is_some_and(|i| !i.is_empty()),
            "{created}"
        );
        introduction = created["introduction"].as_str().unwrap().to_owned();
        let refused = c
            .call("bot.delete", json!({"profile": "default", "confirm": true}))
            .await;
        assert!(refused.is_err());
    });
    let secrets = ok(&run(home.path(), &["-p", "scout", "secrets", "list"]));
    assert!(secrets.contains("OPENROUTER_API_KEY"), "{secrets}");
    assert!(!secrets.contains("OAUTH_ANTHROPIC"), "{secrets}");
    let chat = bot_chat(home.path(), "scout").expect("the new bot's Bot Chat");
    assert_eq!(
        said(home.path(), "scout", &chat, "assistant"),
        [introduction]
    );
    runtime.block_on(async {
        let c = client(home.path(), &["default", "scout"]).await;
        let copy = call(
            &c,
            "bot.duplicate",
            json!({"profile": "scout", "name": "scout-2"}),
        )
        .await;
        assert_eq!(copy["profile"], "scout-2");
        let c = client(home.path(), &["default", "scout", "scout-2"]).await;
        let meta = call(&c, "bot.meta.get", json!({"profile": "scout-2"})).await;
        assert_eq!(meta["title"], "Scout");
        call(
            &c,
            "bot.delete",
            json!({"profile": "scout-2", "confirm": true}),
        )
        .await;
    });
    let listed = ok(&run(home.path(), &["profile", "list"]));
    assert!(
        listed.contains("scout") && !listed.contains("scout-2"),
        "{listed}"
    );
}

/// Scenario: /new keeps the relationship.
#[test]
fn new_in_a_bot_chat_compresses_it_in_place() {
    let home = bots::home("[[rule]]\nrepeat = true\ntext = \"summary of the chat\"\n");
    let id = ok(&run(
        home.path(),
        &["--json", "chat", "--bot-chat", "-z", "hello"],
    ));
    let id = serde_json::from_str::<Value>(&id).unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let store = store(home.path(), "default");
    let rows: Vec<pmt_store::NewMessage> = (0..300)
        .map(|n| {
            let role = if n % 2 == 0 {
                pmt_store::Role::User
            } else {
                pmt_store::Role::Assistant
            };
            pmt_store::NewMessage::new(
                role,
                format!("message {n}: {}", "a long line of chat ".repeat(20)),
            )
        })
        .collect();
    store.append_messages(id.parse().unwrap(), &rows).unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut notice = String::new();
    runtime.block_on(async {
        let c = client(home.path(), &["default"]).await;
        call(&c, "session.attach", json!({"session_id": id})).await;
        let done = call(
            &c,
            "command.dispatch",
            json!({"session_id": id, "name": "new", "args": ""}),
        )
        .await;
        assert_eq!(done["type"], "done", "{done}");
        notice = done["notice"].to_string();
        let again = call(&c, "bot.chat.ensure", json!({"profile": "default"})).await;
        assert_eq!(
            again["session_id"],
            id.as_str(),
            "still one canonical Bot Chat"
        );
    });
    let active = said(home.path(), "default", &id, "user").len()
        + said(home.path(), "default", &id, "assistant").len();
    assert!(
        active < 300,
        "compressed in place: {active} active messages ({notice})"
    );
}

/// Scenario: Two bots, two voices (each bot speaks with its own profile's
/// `tts.*`).
#[test]
fn each_bot_reads_aloud_with_its_own_voice() {
    let home = bots::home("");
    let speaker = |voice: &str| {
        [
            ("tts.provider", "fake".to_owned()),
            (
                "tts.providers.fake.command",
                "printf %s {voice} > {output_path}".to_owned(),
            ),
            ("tts.providers.fake.voice", voice.to_owned()),
        ]
    };
    for (name, voice) in [("alice", "nova"), ("bob", "onyx")] {
        bot(home.path(), name, "", &[]);
        for (key, value) in speaker(voice) {
            ok(&run(
                home.path(),
                &["-p", name, "config", "set", key, &value],
            ));
        }
    }
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        use base64::Engine as _;
        let c = client(home.path(), &["default", "alice", "bob"]).await;
        for (name, voice) in [("alice", "nova"), ("bob", "onyx")] {
            let chat = call(&c, "bot.chat.ensure", json!({"profile": name})).await;
            let spoken = call(
                &c,
                "voice.speak",
                json!({"profile": name, "session_id": chat["session_id"], "text": "hello"}),
            )
            .await;
            let audio = base64::engine::general_purpose::STANDARD
                .decode(spoken["audio"].as_str().unwrap())
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&audio), voice, "{name}");
        }
    });
}

/// Scenario: Morning digest lands in the Bot Chat.
#[test]
fn a_routine_delivers_into_the_bot_chat_and_is_an_ordinary_job() {
    let home = bots::home("");
    bot(
        home.path(),
        "inbox",
        "[[rule]]\nuser = \"digest\"\nrepeat = true\ntext = \"three new mails\"\n\
         [[rule]]\nrepeat = true\ntext = \"filed\"\n",
        &[("cron.tick_interval_secs", "10")],
    );
    let added = ok(&run(
        home.path(),
        &[
            "-p",
            "inbox",
            "--json",
            "cron",
            "add",
            "0 8 * * *",
            "--prompt",
            "digest my mail",
            "--name",
            "[bot:inbox] morning digest",
            "--deliver",
            "bot_chat",
            "--yes",
        ],
    ));
    let job = serde_json::from_str::<Value>(&added).unwrap()["job"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let now = jiff::Timestamp::now().as_millisecond();
    store(home.path(), "inbox")
        .cron_modify_job(&job, |row| {
            row.next_run_at = Some(now - 1_000);
            Ok(())
        })
        .unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let told = wait_said(home.path(), "inbox", "user", "three new mails");
    assert!(
        told.iter()
            .any(|t| t.contains("[Scheduled: [bot:inbox] morning digest]")),
        "{told:?}"
    );
    wait("the run is listed", Duration::from_mins(1), || {
        ok(&run(home.path(), &["-p", "inbox", "cron", "runs", &job])).contains("ok")
    });
    let listed = ok(&run(home.path(), &["-p", "inbox", "cron", "list"]));
    assert!(listed.contains("[bot:inbox] morning digest"), "{listed}");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let routines = runtime.block_on(async {
        let c = client(home.path(), &["default", "inbox"]).await;
        call(&c, "bot.routines.list", json!({"profile": "inbox"})).await
    });
    assert_eq!(
        routines["routines"][0]["routine"], "morning digest",
        "{routines}"
    );
    assert_eq!(routines["routines"][0]["schedule"], "0 8 * * *");
}

/// The terminal equivalents: `chat --bot-chat` opens the same chat the
/// roster opens, and `rpc` reaches every `bot.*` method.
#[test]
fn the_terminal_reaches_the_same_bot_chat_and_metadata() {
    let home = bots::home("[[rule]]\nrepeat = true\ntext = \"ok\"\n");
    let _daemon = daemon::start(home.path(), &[]);
    let shown = ok(&run(
        home.path(),
        &[
            "rpc",
            "call",
            "bot.meta.update",
            r#"{"profile":"default","changes":{"title":"Prime"}}"#,
        ],
    ));
    assert!(shown.contains("Prime"), "{shown}");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let ensured = runtime.block_on(async {
        let c = client(home.path(), &["default"]).await;
        call(&c, "bot.chat.ensure", json!({"profile": "default"})).await
    });
    let out = ok(&run(
        home.path(),
        &["--json", "chat", "--bot-chat", "-z", "hi"],
    ));
    let report: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["session_id"], ensured["session_id"]);
}

/// Scenario: Create on a remote machine.
#[test]
fn a_bot_created_on_a_peer_lives_there() {
    let peer = self::common::api::home("");
    ok(&run(
        peer.path(),
        &["config", "set", "gateway.multiplex", "true"],
    ));
    let home = self::common::api::home("");
    ok(&run(
        home.path(),
        &["config", "set", "gateway.multiplex", "true"],
    ));
    bot(home.path(), "research", "", &[]);
    let _p = daemon::start(peer.path(), &[]);
    let _h = daemon::start(home.path(), &[]);
    let url = format!("http://127.0.0.1:{}", self::common::api::port(peer.path()));
    ok(&run_with_stdin(
        home.path(),
        &["gateway", "peer", "add", "homelab", "--url", &url],
        &format!("{}\n", self::common::api::key_of("default")),
    ));
    let created = ok(&run(
        home.path(),
        &[
            "rpc",
            "call",
            "bot.create",
            r#"{"profile":"default","name":"research","title":"Research","on":"homelab"}"#,
        ],
    ));
    let created: Value = serde_json::from_str(&created).unwrap();
    assert_eq!(created["handle"], "@research-homelab", "{created}");
    assert_eq!(created["device"], "homelab");
    let there = ok(&run(peer.path(), &["profile", "list"]));
    assert!(there.contains("research"), "{there}");
}
