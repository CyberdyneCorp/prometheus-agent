//! The daemon side of the desktop's calls (spec: desktop-app, Sessions are
//! created with desktop source; Slash palette from one command registry;
//! Bot chats are created atomically by the daemon; Settings write validated
//! config patches; Profile switching is explicit), over a real daemon with
//! `desktop` clients.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;

use pmt_rpc::client::Client;
use pmt_rpc::contracts::ClientKind;
use serde_json::{Value, json};

use self::common::daemon::{self, record};
use self::common::{mock_home, ok, run};

async fn desktop(home: &Path, profile: &str) -> Client {
    let mut hello = daemon::hello(true);
    hello.client = ClientKind::Desktop;
    let (client, _inbox) = Client::connect(
        &record(home).endpoint,
        pmt_rpc::control::Purpose::Interactive,
        vec![profile.into()],
        &hello,
    )
    .await
    .unwrap();
    client
}

fn session(home: &Path, id: &str) -> Value {
    let shown: Value =
        serde_json::from_str(&ok(&run(home, &["--json", "sessions", "show", id]))).unwrap();
    shown["session"].clone()
}

#[test]
fn a_desktop_chat_is_created_with_desktop_source() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let id = runtime.block_on(async {
        let client = desktop(home.path(), "default").await;
        let created = client
            .call("session.create", json!({"source": "desktop"}))
            .await
            .unwrap();
        created["session_id"].as_str().unwrap().to_owned()
    });
    assert_eq!(session(home.path(), &id)["source"], "desktop");
}

#[test]
fn the_desktop_catalog_has_terminal_only_reasons_and_subcommand_dispositions() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let catalog = runtime.block_on(async {
        let client = desktop(home.path(), "default").await;
        let created = client.call("session.create", json!({})).await.unwrap();
        client
            .call(
                "commands.catalog",
                json!({"session_id": created["session_id"]}),
            )
            .await
            .unwrap()
    });
    let entry = |name: &str| {
        catalog["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no /{name} in {catalog}"))
    };
    let keys = entry("keys");
    assert_eq!(keys["disposition"], "unavailable");
    assert_eq!(
        keys["reason"],
        "/keys is only available in the terminal interface."
    );
    let skills = entry("skills");
    let pending = skills["subcommands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "pending")
        .cloned()
        .unwrap();
    assert_eq!(pending["disposition"], "exec");
}

#[test]
fn two_windows_opening_the_same_bot_during_a_restart_share_one_bot_chat() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let ensure = || async {
        let client = desktop(home.path(), "default").await;
        let chat = client
            .call("bot.chat.ensure", json!({"profile": "default"}))
            .await
            .unwrap();
        chat["session_id"].as_str().unwrap().to_owned()
    };
    let (first, second) = runtime.block_on(async { tokio::join!(ensure(), ensure()) });
    assert_eq!(first, second);
    ok(&run(home.path(), &["daemon", "restart"]));
    let after = runtime.block_on(ensure());
    assert_eq!(after, first, "the same bot chat after the restart");
    let (listed, hidden) = runtime.block_on(async {
        let client = desktop(home.path(), "default").await;
        let listed = client.call("session.list", json!({})).await.unwrap();
        let hidden = client
            .call("session.list", json!({"include_hidden": true}))
            .await
            .unwrap();
        (listed, hidden)
    });
    let ids = |list: &Value| -> Vec<String> {
        list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["session_id"].as_str().unwrap().to_owned())
            .collect()
    };
    assert!(!ids(&listed).contains(&first), "{listed}");
    assert_eq!(ids(&hidden), vec![first.clone()], "exactly one bot chat");
    assert_eq!(session(home.path(), &first)["bot_chat"], true);
}

#[test]
fn clearing_the_subagent_route_removes_it_in_one_validated_patch() {
    let home = mock_home();
    for (key, value) in [
        ("delegation.model", "mock/mock"),
        ("delegation.base_url", "http://127.0.0.1:9/v1"),
        ("delegation.api_key_secret", "SUB_KEY"),
        ("delegation.wire_format", "chat_completions"),
    ] {
        ok(&run(home.path(), &["config", "set", key, value]));
    }
    let _daemon = daemon::start(home.path(), &[]);
    let config = home.path().join("profiles/default/config.toml");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let client = runtime.block_on(desktop(home.path(), "default"));
    // A schema violation anywhere leaves the file as it was.
    let before = std::fs::read_to_string(&config).unwrap();
    let refused = runtime
        .block_on(client.call(
            "config.patch",
            json!({"profile": "default", "changes": [
                {"key": "delegation.model", "value": null},
                {"key": "delegation.max_iterations", "value": 0},
            ]}),
        ))
        .unwrap_err();
    assert!(
        refused.to_string().contains("delegation.max_iterations"),
        "{refused}"
    );
    assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
    // "Inherit from main agent": one patch removes the whole route.
    let patched = runtime
        .block_on(client.call(
            "config.patch",
            json!({"profile": "default", "changes": [
                {"key": "delegation.model", "value": null},
                {"key": "delegation.base_url", "value": null},
                {"key": "delegation.api_key_secret", "value": null},
                {"key": "delegation.wire_format", "value": null},
            ]}),
        ))
        .unwrap();
    assert_eq!(patched["keys"].as_array().unwrap().len(), 4);
    let after: toml::Table = std::fs::read_to_string(&config).unwrap().parse().unwrap();
    let route = after.get("delegation").and_then(toml::Value::as_table);
    assert!(route.is_none_or(toml::Table::is_empty), "{after}");
    let model = runtime
        .block_on(client.call(
            "config.get",
            json!({"profile": "default", "key": "delegation.model"}),
        ))
        .unwrap();
    assert_eq!(model["value"], Value::Null, "{model}");
}

#[test]
fn a_config_reload_names_the_windows_profile() {
    let home = mock_home();
    ok(&run(home.path(), &["profile", "create", "coder"]));
    ok(&run(
        home.path(),
        &["config", "set", "gateway.multiplex", "true"],
    ));
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let coder = runtime.block_on(desktop(home.path(), "coder"));
    let reloaded = runtime
        .block_on(coder.call("config.reload", json!({"profile": "coder"})))
        .unwrap();
    assert_eq!(reloaded["profile"], "coder");
    let other = runtime
        .block_on(coder.call("config.reload", json!({"profile": "default"})))
        .unwrap_err();
    assert!(other.to_string().contains("not granted"), "{other}");
    let unnamed = runtime
        .block_on(coder.call("config.reload", json!({})))
        .unwrap_err();
    assert!(unnamed.to_string().contains("profile"), "{unnamed}");
}
