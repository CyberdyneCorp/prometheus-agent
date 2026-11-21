//! Bot Mode stays inside the caller's grant (spec: bot-mode, Bot
//! profiles and creation; Group chats; Hosted rooms across gateways): an
//! RPC connection granted one profile cannot clone, grant or put another
//! in a room, nor reach a room that runs turns as one; an API key of one
//! profile cannot clone another, read a room it is not in or poll a run
//! without the room grant.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;

use serde_json::{Value, json};

use self::common::bots::{self, bot, client};
use self::common::{api, daemon, ok, run};

fn rpc(home: &Path, method: &str, params: &Value) -> Value {
    bots::admin_call(home, method, params).unwrap_or_else(|e| panic!("{method}: {e}"))
}

/// The error code `method` is refused with.
async fn refused(client: &pmt_rpc::client::Client, method: &str, params: Value) -> String {
    let err = client
        .call(method, params.clone())
        .await
        .expect_err(&format!("{method} {params} succeeded"));
    pmt_rpc::client::error_of(&err).code().code.to_owned()
}

#[test]
fn a_connection_granted_one_bot_cannot_act_as_another() {
    let home = bots::home("[[rule]]\nrepeat = true\ntext = \"hi\"\n");
    for name in ["ann", "bob"] {
        bot(
            home.path(),
            name,
            "[[rule]]\nrepeat = true\ntext = \"hi\"\n",
            &[],
        );
    }
    let _daemon = daemon::start(home.path(), &[]);
    let room = rpc(
        home.path(),
        "groups.create",
        &json!({"name": "Pair", "members": ["ann", "bob"]}),
    )["room_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let ann = client(home.path(), &["ann"]).await;
        let not_granted = "profile.not_granted";
        let grant = json!({"profile": "ann", "room_id": room, "authority": "gw_x", "member": "bob", "scopes": ["post"]});
        assert_eq!(refused(&ann, "groups.grant", grant).await, not_granted);
        let clone = json!({"profile": "ann", "name": "copy", "clone_from": "bob"});
        assert_eq!(refused(&ann, "bot.create", clone).await, not_granted);
        let create = json!({"profile": "ann", "name": "Mine", "members": ["ann", "bob"]});
        assert_eq!(refused(&ann, "groups.create", create).await, not_granted);
        let log = json!({"profile": "ann", "room_id": room});
        assert_eq!(refused(&ann, "groups.log", log.clone()).await, not_granted);
        let post = json!({"profile": "ann", "room_id": room, "text": "hi"});
        assert_eq!(refused(&ann, "groups.post", post).await, not_granted);
        let listed = ann
            .call("groups.list", json!({"profile": "ann"}))
            .await
            .unwrap();
        assert_eq!(listed["rooms"], json!([]), "{listed}");
    });
    assert!(!home.path().join("profiles/copy").exists());
}

#[test]
fn an_api_key_reaches_only_its_own_profile() {
    let home = api::home("");
    api::multiplex(home.path());
    api::profile(home.path(), "foo", "");
    ok(&run(
        home.path(),
        &["config", "set", "providers.mock.script", "mock.toml"],
    ));
    std::fs::write(
        home.path().join("profiles/default/mock.toml"),
        "[[rule]]\nrepeat = true\ntext = \"hi\"\n",
    )
    .unwrap();
    for name in ["ann", "bob"] {
        bot(
            home.path(),
            name,
            "[[rule]]\nrepeat = true\ntext = \"hi\"\n",
            &[],
        );
    }
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());

    let clone = json!({"profile": "foo", "name": "stolen", "clone_from": "default"});
    let created = api::Request::new("POST", "/p/foo/api/bots")
        .auth("foo")
        .json(&clone)
        .send(port);
    assert_eq!(created.status, 403, "{}", created.body);
    assert!(!home.path().join("profiles/stolen").exists());

    let room = rpc(
        home.path(),
        "groups.create",
        &json!({"name": "Pair", "members": ["ann", "bob"]}),
    )["room_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let log = api::Request::new("POST", &format!("/p/foo/api/rooms/{room}/log"))
        .auth("foo")
        .json(&json!({"after_seq": 0}))
        .send(port);
    assert_eq!(log.status, 403, "{}", log.body);
    let owner = api::Request::new("POST", &format!("/api/rooms/{room}/log"))
        .auth("default")
        .json(&json!({"after_seq": 0}))
        .send(port);
    assert_eq!(owner.status, 200, "{}", owner.body);
    let state = api::Request::new("GET", &format!("/p/foo/api/rooms/{room}/turns/run_x"))
        .auth("foo")
        .send(port);
    assert_eq!(state.status, 403, "{}", state.body);

    // With the room grant: a run needs its id, and only this room's runs
    // are answered.
    let grant = rpc(
        home.path(),
        "groups.grant",
        &json!({"room_id": room, "authority": "gw_peer", "member": "ann", "scopes": ["post"]}),
    );
    let token = grant["token"].as_str().unwrap();
    let anonymous = api::Request::new("POST", &format!("/p/foo/api/rooms/{room}/turns"))
        .auth("foo")
        .header("X-Prometheus-Room-Grant", token)
        .json(&json!({"member": "ann", "text": "hi"}))
        .send(port);
    assert_eq!(anonymous.status, 400, "{}", anonymous.body);
    let unknown = api::Request::new("GET", &format!("/p/foo/api/rooms/{room}/turns/run_x"))
        .auth("foo")
        .header("X-Prometheus-Room-Grant", token)
        .send(port);
    assert_eq!(unknown.status, 404, "{}", unknown.body);
}
