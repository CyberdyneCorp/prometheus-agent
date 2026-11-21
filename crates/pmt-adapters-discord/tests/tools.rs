//! The Discord tools (spec: platform-adapters, Discord tools): the
//! `server_actions` allowlist filters both schemas and is enforced when a
//! call runs, member actions need the Server Members intent, and calls go
//! to the REST API with the bot token.

use std::collections::BTreeSet;
use std::sync::Arc;

use pmt_adapters_discord::{DiscordTools, Endpoints};
use pmt_config::schema::PlatformConfig;
use pmt_core::cancel::CancelToken;
use pmt_tools::RegisteredTool;
use pmt_tools::descriptor::{CallContext, Executor};
use pmt_tools::platform_tools::{Context, Contributor, SecretReader};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config(options: &Value) -> PlatformConfig {
    serde_json::from_value(json!({"enabled": true, "options": options})).unwrap()
}

fn tools(api: &str, options: &Value, granted: Option<BTreeSet<String>>) -> Vec<RegisteredTool> {
    let dir = tempfile::tempdir().unwrap();
    let config = config(options);
    let secrets: SecretReader =
        Arc::new(|name| (name == "DISCORD_BOT_TOKEN").then(|| "tok".to_owned()));
    let context = Context {
        profile_dir: dir.path(),
        config: &config,
        granted,
        secrets,
        network: &pmt_net::http::OutboundPolicy::default(),
    };
    DiscordTools::new(Endpoints {
        api: api.to_owned(),
    })
    .tools(&context)
}

fn find<'a>(tools: &'a [RegisteredTool], name: &str) -> &'a RegisteredTool {
    tools
        .iter()
        .find(|t| t.name() == name)
        .unwrap_or_else(|| panic!("no tool {name}"))
}

async fn call(tool: &RegisteredTool, arguments: Value) -> Result<String, String> {
    let Executor::Handler(handler) = &tool.executor else {
        panic!("not a handler tool");
    };
    let context = CallContext {
        call_id: "c1".into(),
        session_id: "s1".into(),
        cancel: CancelToken::new(),
    };
    handler
        .call(&context, arguments)
        .await
        .map(|r| serde_json::to_string(&r.content).unwrap())
        .map_err(|f| f.message)
}

/// Scenario: Action not allowlisted.
#[tokio::test]
async fn an_action_outside_server_actions_is_refused_and_hidden() {
    let all = tools(
        "http://127.0.0.1:9",
        &json!({"server_actions": ["fetch_messages", "list_channels"]}),
        None,
    );
    let admin = find(&all, "discord_admin");
    assert!(admin.descriptor.description.contains("list_channels"));
    assert!(!admin.descriptor.description.contains("add_role"));
    let channel = find(&all, "discord");
    assert!(!channel.descriptor.description.contains("create_thread"));
    let err = call(
        admin,
        json!({"action": "add_role", "guild_id": "1", "user_id": "2", "role_id": "3"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err, "action add_role disabled by config");
}

#[tokio::test]
async fn member_actions_need_the_server_members_intent() {
    let without = tools("http://127.0.0.1:9", &json!({}), Some(BTreeSet::new()));
    assert!(
        !find(&without, "discord_admin")
            .descriptor
            .description
            .contains("member_info")
    );
    assert!(
        !find(&without, "discord")
            .descriptor
            .description
            .contains("search_members")
    );
    // A failed detection (unknown) shows every action.
    let unknown = tools("http://127.0.0.1:9", &json!({}), None);
    assert!(
        find(&unknown, "discord_admin")
            .descriptor
            .description
            .contains("member_info")
    );
    assert!(
        find(&unknown, "discord")
            .descriptor
            .description
            .contains("search_members")
    );
}

#[tokio::test]
async fn actions_call_the_rest_api_with_the_bot_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/channels/123/messages"))
        .and(query_param("limit", "50"))
        .and(header("authorization", "Bot tok"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!([{"id": "9", "content": "hi"}])),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/channels/123/threads"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "77", "name": "plan"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/guilds/1/members/2/roles/3"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let all = tools(&server.uri(), &json!({}), None);
    let channel = find(&all, "discord");
    let fetched = call(
        channel,
        json!({"action": "fetch_messages", "channel_id": "123"}),
    )
    .await
    .unwrap();
    assert!(fetched.contains("hi"), "{fetched}");
    let thread = call(
        channel,
        json!({"action": "create_thread", "channel_id": "123", "name": "plan"}),
    )
    .await
    .unwrap();
    assert!(thread.contains("77"), "{thread}");
    let admin = find(&all, "discord_admin");
    let added = call(
        admin,
        json!({"action": "add_role", "guild_id": "1", "user_id": "2", "role_id": "3"}),
    )
    .await
    .unwrap();
    assert!(added.contains("add_role: done"), "{added}");
    let requests = server.received_requests().await.unwrap();
    let thread_body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(thread_body["auto_archive_duration"], 1440);
}

#[tokio::test]
async fn every_tool_action_has_external_effect() {
    for tool in tools("http://127.0.0.1:9", &json!({}), None) {
        assert_eq!(
            tool.descriptor.effect,
            pmt_tools::descriptor::Effect::External
        );
    }
}
