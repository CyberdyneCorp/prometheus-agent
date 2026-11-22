//! Group chats (spec: bot-mode, Group chats; Room stop directives and
//! held members; Durable room driver; Hosted rooms across gateways; Room
//! authority transfer): rooms of mock bots driven by the real daemon, and
//! a room hosted across two daemons.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use self::common::bots::{self, bot, store, wait};
use self::common::{api, daemon, ok, run, run_with_stdin};

fn rpc(home: &Path, method: &str, params: &Value) -> Value {
    bots::admin_call(home, method, params).unwrap_or_else(|e| panic!("{method}: {e}"))
}

fn rpc_refused(home: &Path, method: &str, params: &Value) -> String {
    bots::admin_call(home, method, params).expect_err(&format!("{method} succeeded"))
}

/// The room log once its discussion settled (or stopped).
fn settled(home: &Path, room: &str) -> Value {
    let mut log = Value::Null;
    wait("the room settles", Duration::from_mins(2), || {
        log = rpc(home, "groups.log", &json!({"room_id": room}));
        log["entries"]
            .as_array()
            .unwrap()
            .last()
            .is_some_and(|e| e["kind"] == "settled" || e["kind"] == "stopped")
    });
    log
}

fn kinds<'a>(log: &'a Value, kind: &str) -> Vec<&'a Value> {
    log["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == kind)
        .collect()
}

/// A home with bots `names`, each answering with `rules`.
fn room_home(names: &[&str], rules: &str) -> common::TestHome {
    let home = bots::home("[[rule]]\nrepeat = true\ntext = \"hi\"\n");
    for name in names {
        bot(home.path(), name, rules, &[]);
    }
    home
}

fn create(home: &Path, name: &str, members: &[&str]) -> String {
    let room = rpc(
        home,
        "groups.create",
        &json!({"name": name, "members": members}),
    );
    room["room_id"].as_str().unwrap().to_owned()
}

/// Scenario: Rooms do not spin. Scenario: Client closed mid-discussion
/// (every post comes from a client that disconnects at once).
#[test]
fn a_room_of_chatty_bots_stops_at_its_bounds() {
    let names = ["ann", "ben", "cat", "dan"];
    let home = room_home(
        &names,
        "[[rule]]\nrepeat = true\ntext = \"I have more to say\"\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let room = create(home.path(), "Chatter", &names);
    let capabilities = rpc(
        home.path(),
        "groups.capabilities",
        &json!({"room_id": room}),
    );
    assert_eq!(capabilities["driver"], true, "{capabilities}");
    rpc(
        home.path(),
        "groups.post",
        &json!({"room_id": room, "text": "what do you think?"}),
    );
    let log = settled(home.path(), &room);
    let said: Vec<&Value> = kinds(&log, "message")
        .into_iter()
        .filter(|e| e["author"] != "user")
        .collect();
    assert_eq!(said.len(), 10, "{log}");
    assert!(kinds(&log, "round").len() <= 3);
    assert_eq!(kinds(&log, "turn.started").len(), 10);
    let session = store(home.path(), "ann").list_sessions(&pmt_store::ListOptions {
        all: true,
        ..pmt_store::ListOptions::default()
    });
    let titles: Vec<Option<String>> = session.unwrap().into_iter().map(|s| s.title).collect();
    assert!(
        titles.contains(&Some("Group: Chatter · main".to_owned())),
        "{titles:?}"
    );
}

/// Scenario: Silent round settles.
#[test]
fn a_fully_silent_round_settles_the_room() {
    let names = ["ann", "ben"];
    let home = room_home(&names, "[[rule]]\nrepeat = true\ntext = \"[PASS]\"\n");
    let _daemon = daemon::start(home.path(), &[]);
    let room = create(home.path(), "Quiet", &names);
    rpc(
        home.path(),
        "groups.post",
        &json!({"room_id": room, "text": "anyone?"}),
    );
    let log = settled(home.path(), &room);
    assert_eq!(kinds(&log, "round").len(), 1, "{log}");
    assert_eq!(kinds(&log, "pass").len(), 2);
    assert_eq!(kinds(&log, "settled")[0]["data"]["rounds"], 1);
}

/// Scenario: Release delivers skipped messages; a renamed room keeps its
/// history and a taken name is refused.
#[test]
fn a_held_member_hears_what_it_missed_when_released() {
    let home = room_home(&[], "");
    bot(
        home.path(),
        "builder",
        "[[rule]]\nrepeat = true\ntext = \"built\"\n",
        &[],
    );
    bot(
        home.path(),
        "critic",
        "[[rule]]\nrepeat = true\ntext = \"[PASS]\"\n",
        &[],
    );
    let _daemon = daemon::start(home.path(), &[]);
    let room = create(home.path(), "Workshop", &["builder", "critic"]);
    let posted = rpc(
        home.path(),
        "groups.post",
        &json!({"room_id": room, "text": "@builder stop"}),
    );
    assert_eq!(posted["held"], json!(["builder"]));
    settled(home.path(), &room);
    for text in ["first idea", "second idea", "third idea"] {
        rpc(
            home.path(),
            "groups.post",
            &json!({"room_id": room, "text": text}),
        );
        settled(home.path(), &room);
    }
    let renamed = rpc(
        home.path(),
        "groups.update",
        &json!({"room_id": room, "name": "Atelier"}),
    );
    assert_eq!(renamed["name"], "Atelier");
    create(home.path(), "Other", &["builder", "critic"]);
    let taken = rpc_refused(
        home.path(),
        "groups.update",
        &json!({"room_id": room, "name": "Other"}),
    );
    assert!(taken.contains("already exists"), "{taken}");
    rpc(
        home.path(),
        "groups.post",
        &json!({"room_id": room, "text": "@all continue"}),
    );
    let log = settled(home.path(), &room);
    assert!(
        kinds(&log, "message")
            .iter()
            .any(|e| e["author"] == "builder"),
        "{log}"
    );
    let room_sessions = |profile: &str| -> Vec<pmt_store::Session> {
        store(home.path(), profile)
            .list_sessions(&pmt_store::ListOptions {
                all: true,
                ..pmt_store::ListOptions::default()
            })
            .unwrap()
            .into_iter()
            .filter(|s| s.title.as_deref().is_some_and(|t| t.starts_with("Group: ")))
            .collect()
    };
    // The critic's room session, from before the rename, is still its one.
    let critic = room_sessions("critic");
    assert_eq!(critic.len(), 1);
    assert_eq!(critic[0].title.as_deref(), Some("Group: Workshop · main"));
    let builder = room_sessions("builder");
    let session = builder.first().expect("the builder's room session");
    let asked = bots::said(home.path(), "builder", &session.id.to_string(), "user");
    // Its first turn after the release.
    let turn = asked.first().unwrap();
    for text in ["first idea", "second idea", "third idea", "@all continue"] {
        assert!(turn.contains(text), "{turn}");
    }
}

/// Scenario: Promotion without confirmation. Scenario: Demoted authority
/// refuses sends.
#[test]
fn promotion_needs_confirmation_and_a_demoted_authority_refuses_sends() {
    let home = room_home(
        &["ann", "ben"],
        "[[rule]]\nrepeat = true\ntext = \"[PASS]\"\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let room = create(home.path(), "Ops", &["ann", "ben"]);
    let before = rpc(
        home.path(),
        "groups.replica_state",
        &json!({"room_id": room}),
    );
    let refused = rpc_refused(home.path(), "groups.promote", &json!({"room_id": room}));
    assert!(refused.contains("confirmation_required"), "{refused}");
    assert_eq!(
        rpc(
            home.path(),
            "groups.replica_state",
            &json!({"room_id": room})
        ),
        before
    );
    let demoted = rpc(
        home.path(),
        "groups.demote",
        &json!({"room_id": room, "observed_gateway_id": "gw_new", "observed_epoch": 2}),
    );
    assert_eq!(demoted["role"], "demoted");
    let again = rpc(
        home.path(),
        "groups.demote",
        &json!({"room_id": room, "observed_gateway_id": "gw_new", "observed_epoch": 2}),
    );
    assert_eq!(again, demoted, "demotion is idempotent");
    let refused = rpc_refused(
        home.path(),
        "groups.post",
        &json!({"room_id": room, "text": "hi"}),
    );
    assert!(refused.contains("room authority is gw_new"), "{refused}");
    let log = rpc(home.path(), "groups.log", &json!({"room_id": room}));
    assert_eq!(kinds(&log, "authority.lost").len(), 1);
}

/// Registers peer `name` at `url` with `key` in `home`.
fn add_peer(home: &Path, name: &str, url: &str, key: &str) {
    ok(&run_with_stdin(
        home,
        &["gateway", "peer", "add", name, "--url", url],
        &format!("{key}\n"),
    ));
}

/// Scenario: Remote member without a grant; with the member gateway's
/// grant the member's turn runs on its own machine; a replica copies the
/// log, is promoted with confirmation and the old authority is demoted.
#[test]
fn a_room_hosted_across_two_gateways() {
    let member = api::home("");
    std::fs::write(
        member.path().join("profiles/default/mock.toml"),
        "[[rule]]\nrepeat = true\ntext = \"remote view\"\n",
    )
    .unwrap();
    ok(&run(
        member.path(),
        &["config", "set", "providers.mock.script", "mock.toml"],
    ));
    let authority = api::home("");
    ok(&run(
        authority.path(),
        &["config", "set", "gateway.multiplex", "true"],
    ));
    bot(
        authority.path(),
        "ann",
        "[[rule]]\nrepeat = true\ntext = \"[PASS]\"\n",
        &[],
    );
    let _m = daemon::start(member.path(), &[]);
    let _a = daemon::start(authority.path(), &[]);
    let url = |home: &Path| format!("http://127.0.0.1:{}", api::port(home));
    add_peer(
        authority.path(),
        "homelab",
        &url(member.path()),
        &api::key_of("default"),
    );
    add_peer(
        member.path(),
        "hq",
        &url(authority.path()),
        &api::key_of("default"),
    );
    let room = create(authority.path(), "Hosted", &["ann", "homelab/default"]);
    rpc(
        authority.path(),
        "groups.post",
        &json!({"room_id": room, "text": "status?"}),
    );
    settled(authority.path(), &room);
    let shown = rpc(authority.path(), "groups.get", &json!({"room_id": room}));
    let remote = &shown["members"][1];
    assert_eq!(
        (remote["state"].as_str(), remote["reason"].as_str()),
        (Some("unreachable"), Some("missing_config")),
        "{shown}"
    );

    let gateway = shown["authority_gateway_id"].as_str().unwrap();
    let grant = rpc(
        member.path(),
        "groups.grant",
        &json!({"room_id": room, "authority": gateway, "member": "default", "scopes": ["post"]}),
    );
    rpc(
        authority.path(),
        "groups.member_grant",
        &json!({"room_id": room, "member": "homelab/default", "token": grant["token"]}),
    );
    rpc(
        authority.path(),
        "groups.post",
        &json!({"room_id": room, "text": "status now?"}),
    );
    let log = settled(authority.path(), &room);
    assert!(
        kinds(&log, "message")
            .iter()
            .any(|e| e["author"] == "homelab/default" && e["text"] == "remote view"),
        "{log}"
    );

    let state = rpc(
        member.path(),
        "groups.replicate",
        &json!({"room_id": room, "from": "hq"}),
    );
    assert_eq!(state["last_seq"], state["latest_seq"], "{state}");
    let promoted = rpc(
        member.path(),
        "groups.promote",
        &json!({"room_id": room, "confirm": true}),
    );
    assert_eq!(promoted["authority_epoch"], 2);
    rpc(
        authority.path(),
        "groups.demote",
        &json!({"room_id": room, "observed_gateway_id": promoted["authority_gateway_id"], "observed_epoch": 2}),
    );
    let refused = rpc_refused(
        authority.path(),
        "groups.post",
        &json!({"room_id": room, "text": "hi"}),
    );
    assert!(refused.contains("room authority is gw_"), "{refused}");
}
