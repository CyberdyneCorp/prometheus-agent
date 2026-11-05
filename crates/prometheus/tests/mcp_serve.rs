//! `prometheus mcp serve` against the reference MCP client (spec:
//! plugin-system, MCP server mode): what is exposed by default, skills as
//! prompts and resources, calls under the profile's approval policy, and
//! the HTTP transport's loopback rule and bearer token.

mod common;

use std::io::BufRead as _;
use std::path::Path;
use std::process::Stdio;

use rmcp::ServiceExt as _;
use rmcp::model::{
    CallToolRequestParams, GetPromptRequestParams, ReadResourceRequestParams, ResourceContents,
};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::json;

use self::common::{command, ok, run, stderr, temp_home};

/// A home whose default profile has skill `demo`.
fn home_with_skill() -> tempfile::TempDir {
    let home = temp_home();
    ok(&run(home.path(), &["config", "path"]));
    let skill = home.path().join("profiles/default/skills/demo/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(
        &skill,
        "---\nname: demo\ndescription: A demo skill.\n---\nSay the demo words.\n",
    )
    .unwrap();
    home
}

type Client = RunningService<RoleClient, ()>;

/// `mcp serve --stdio` with the reference client attached.
async fn stdio(home: &Path) -> (Client, tokio::process::Child) {
    let mut cmd = tokio::process::Command::from(command(home));
    cmd.args(["mcp", "serve", "--stdio"])
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd.spawn().unwrap();
    let io = (child.stdout.take().unwrap(), child.stdin.take().unwrap());
    let client = ().serve(io).await.unwrap();
    (client, child)
}

fn text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match c {
            rmcp::model::ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn call(
    client: &Client,
    name: &str,
    arguments: serde_json::Value,
) -> rmcp::model::CallToolResult {
    let serde_json::Value::Object(arguments) = arguments else {
        unreachable!()
    };
    client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(arguments))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn terminal_not_exposed_by_default() {
    let home = home_with_skill();
    let (client, _child) = stdio(home.path()).await;
    let mut names: Vec<String> = client
        .peer()
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    names.sort();
    assert_eq!(names, ["skill_view", "skills_list"]);

    let listed = call(&client, "skills_list", json!({})).await;
    assert!(text(&listed).contains("demo"), "{}", text(&listed));

    let prompts = client.peer().list_all_prompts().await.unwrap();
    let demo = prompts.iter().find(|p| p.name == "demo").unwrap();
    assert_eq!(demo.description.as_deref(), Some("A demo skill."));
    let prompt = client
        .get_prompt(GetPromptRequestParams::new("demo"))
        .await
        .unwrap();
    let body = serde_json::to_string(&prompt.messages).unwrap();
    assert!(body.contains("Say the demo words."), "{body}");

    let resources = client.peer().list_all_resources().await.unwrap();
    assert!(resources.iter().any(|r| r.uri == "skill://demo"));
    let read = client
        .read_resource(ReadResourceRequestParams::new("skill://demo"))
        .await
        .unwrap();
    let ResourceContents::TextResourceContents { text, .. } = &read.contents[0] else {
        panic!("not text: {read:?}");
    };
    assert!(text.contains("Say the demo words."), "{text}");
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn exposed_terminal_calls_run_under_the_approval_policy_and_are_denied_when_they_need_a_person()
 {
    let home = home_with_skill();
    ok(&run(
        home.path(),
        &[
            "config",
            "set",
            "mcp_server.expose_toolsets",
            "[\"terminal\"]",
        ],
    ));
    let (client, _child) = stdio(home.path()).await;
    let names: Vec<String> = client
        .peer()
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    assert!(names.contains(&"terminal".to_owned()), "{names:?}");
    assert!(!names.contains(&"skills_list".to_owned()), "{names:?}");
    let safe = call(&client, "terminal", json!({"command": "echo served"})).await;
    assert_ne!(safe.is_error, Some(true), "{}", text(&safe));
    assert!(text(&safe).contains("served"), "{}", text(&safe));
    let needs_approval = call(&client, "terminal", json!({"command": "git reset --hard"})).await;
    assert_eq!(needs_approval.is_error, Some(true));
    assert!(
        text(&needs_approval).starts_with("BLOCKED:"),
        "{}",
        text(&needs_approval)
    );
    client.cancel().await.unwrap();
}

#[test]
fn http_refuses_a_non_loopback_address_unless_remote_serving_is_allowed() {
    let home = temp_home();
    let out = run(home.path(), &["mcp", "serve", "--http", "0.0.0.0:0"]);
    assert_eq!(out.status.code(), Some(77), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("refusing to serve MCP on 0.0.0.0:0: it is not a loopback address"),
        "{}",
        stderr(&out)
    );
}

/// `mcp serve --http 127.0.0.1:0` and the URL it serves.
fn serve_http(home: &Path) -> (std::process::Child, String) {
    let mut child = command(home)
        .args(["mcp", "serve", "--http", "127.0.0.1:0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufReader::new(child.stderr.take().unwrap()).lines();
    let url = loop {
        let line = lines.next().unwrap().unwrap();
        if let Some(rest) = line.strip_prefix("serving MCP on ") {
            break rest.split_whitespace().next().unwrap().to_owned();
        }
    };
    (child, url)
}

fn stop(mut child: std::process::Child) {
    child.kill().unwrap();
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn http_serves_only_requests_with_the_bearer_token_generated_on_first_use() {
    let home = home_with_skill();
    let (first, _) = serve_http(home.path());
    let stored = ok(&run(home.path(), &["secrets", "get", "MCP_SERVER_TOKEN"]));
    assert!(stored.starts_with("MCP_SERVER_TOKEN"), "{stored}");
    stop(first);
    // A token of the user's own replaces the generated one.
    let token = "test-token-0123456789abcdefghijklmnop";
    common::run_with_stdin(home.path(), &["secrets", "set", "MCP_SERVER_TOKEN"], token);
    let (child, url) = serve_http(home.path());
    let client = pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap();
    let anonymous = client
        .post(format!("{url}mcp"))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status().as_u16(), 401);
    let wrong =
        StreamableHttpClientTransportConfig::with_uri(format!("{url}mcp")).auth_header("nope");
    assert!(
        ().serve(StreamableHttpClientTransport::with_client(
            client.clone(),
            wrong
        ))
        .await
        .is_err()
    );
    let config =
        StreamableHttpClientTransportConfig::with_uri(format!("{url}mcp")).auth_header(token);
    let mcp = ().serve(StreamableHttpClientTransport::with_client(client, config)).await.unwrap();
    let tools = mcp.peer().list_all_tools().await.unwrap();
    assert!(tools.iter().any(|t| t.name == "skills_list"));
    mcp.cancel().await.unwrap();
    stop(child);
}
