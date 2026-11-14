//! Task 66.2 conformance fixtures: initialize, authentication, new, load,
//! resume, list and fork sessions, and editor MCP servers owned by the
//! daemon session (spec: acp-adapter).

use pmt_config::{Home as Layout, Launch, Overrides, ProfileName, ProfileScope};
use pmt_store::{LineageKind, NewMessage, NewSession, Role, Source};
use serde_json::{Value, json};

use super::common::{self, daemon};
use super::fixture::{Acp, Home};

fn store(home: &std::path::Path) -> pmt_store::Store {
    let root = Layout::open(home).unwrap();
    let profile = root.profile(&ProfileName::default_profile()).unwrap();
    let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
    pmt_store::Store::open(&scope).unwrap()
}

/// An editor MCP server definition (`true` exits at once: never used).
fn db_server() -> Value {
    json!({"name": "db", "command": "true", "args": [], "env": []})
}

/// Scenario: Version agrees across surfaces; the capabilities and the
/// authentication methods of `initialize`.
#[test]
fn initialize_negotiates_version_capabilities_and_sign_in() {
    let home = Home::new();
    let mut acp = Acp::spawn(home.path(), &[], &[], &[]);
    let answer = acp.initialize(&json!({"fs": {"readTextFile": true, "writeTextFile": true}}));
    assert_eq!(answer["protocolVersion"], 1);
    assert_eq!(answer["agentInfo"]["name"], "prometheus");
    let version = common::ok(&common::run(home.path(), &["--version"]));
    let shown = answer["agentInfo"]["version"].as_str().unwrap();
    assert!(
        version.starts_with(&format!("prometheus {shown} ")),
        "{version} vs {shown}"
    );
    let capabilities = &answer["agentCapabilities"];
    assert_eq!(capabilities["loadSession"], true);
    assert_eq!(capabilities["promptCapabilities"]["image"], true);
    for session in ["list", "fork", "resume"] {
        assert!(
            capabilities["sessionCapabilities"][session].is_object(),
            "{answer}"
        );
    }
    let methods: Vec<&str> = answer["authMethods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(methods, ["prometheus-setup", "mock"]);
    assert_eq!(answer["authMethods"][0]["args"], json!(["setup"]));
    assert_eq!(
        acp.ok("authenticate", &json!({"methodId": "mock"})),
        json!({})
    );
}

/// Scenario: No credentials yet.
#[test]
fn without_a_credential_only_setup_is_offered_and_it_fails() {
    let home = common::temp_home();
    for (key, value) in [("model.provider", "openai"), ("model.default", "gpt-5")] {
        common::ok(&common::run(home.path(), &["config", "set", key, value]));
    }
    let mut acp = Acp::spawn(home.path(), &[], &[], &[]);
    let answer = acp.initialize(&json!({}));
    let methods = answer["authMethods"].as_array().unwrap();
    assert_eq!(methods.len(), 1, "{answer}");
    assert_eq!(methods[0]["id"], "prometheus-setup");
    let refused = acp.call("authenticate", &json!({"methodId": "prometheus-setup"}));
    assert!(refused.get("error").is_some(), "{refused}");
}

/// Scenario: Successful creation. The daemon session has source `acp`
/// and the editor's cwd; the reply carries mode `default`, then the
/// commands and a usage update follow.
#[test]
fn a_new_session_is_an_acp_daemon_session_in_the_editor_cwd() {
    let home = Home::new();
    let _daemon = daemon::start(home.path(), &[]);
    let app = home.work.join("app");
    std::fs::create_dir_all(&app).unwrap();
    let mut acp = Acp::spawn(home.path(), &[], &[], &[]);
    acp.initialize(&json!({}));
    let answer = acp.ok("session/new", &json!({"cwd": app, "mcpServers": []}));
    assert_eq!(answer["modes"]["currentModeId"], "default");
    let session = answer["sessionId"].as_str().unwrap().to_owned();
    let listed = home.rpc("session.list", &json!({"source": "acp"}));
    let row = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == session)
        .cloned()
        .expect("listed as an acp session");
    assert_eq!(row["cwd"], app.to_string_lossy().as_ref());
    acp.wait_for("the commands", |f| {
        f["params"]["update"]["sessionUpdate"] == "available_commands_update"
    });
    acp.wait_for("a usage update", |f| {
        f["params"]["update"]["sessionUpdate"] == "usage_update"
    });
}

/// Scenario: Reopen after compression. The editor holds the id of a
/// session continued after compression; loading it attaches the
/// continuation and addresses everything to the editor's id.
#[test]
fn loading_a_compressed_session_attaches_its_continuation() {
    let home = Home::new();
    let (parent, child) = {
        let store = store(home.path());
        let mut new = NewSession::new(Source::Acp);
        new.cwd = Some(home.work.to_string_lossy().into_owned());
        let parent = store.create_session(&new).unwrap().id;
        store
            .append_message(parent, &NewMessage::new(Role::User, "before compression"))
            .unwrap();
        new.parent = Some((parent, LineageKind::Compression));
        let child = store.create_session(&new).unwrap().id;
        store
            .append_message(child, &NewMessage::new(Role::User, "summary"))
            .unwrap();
        store
            .append_message(child, &NewMessage::new(Role::Assistant, "continued here"))
            .unwrap();
        (parent.to_string(), child.to_string())
    };
    let _daemon = daemon::start(home.path(), &[]);
    let mut acp = Acp::spawn(home.path(), &[], &[], &[]);
    acp.initialize(&json!({}));
    acp.ok(
        "session/load",
        &json!({"sessionId": parent, "cwd": home.work, "mcpServers": []}),
    );
    assert!(
        acp.agent_text(&parent).contains("continued here"),
        "{:?}",
        acp.seen
    );
    let id = acp.send_prompt(&parent, "next");
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
    let transcript = home.rpc("session.transcript", &json!({"session_id": child}));
    let texts: Vec<&str> = transcript["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["text"].as_str())
        .collect();
    assert!(
        texts.contains(&"mock: next"),
        "the turn ran in the continuation: {texts:?}"
    );
    assert!(acp.agent_text(&parent).contains("mock: next"));
}

/// Scenario: Second editor with no servers.
#[test]
fn a_second_editor_with_no_servers_keeps_the_bound_ones() {
    let home = Home::new();
    let _daemon = daemon::start(home.path(), &[]);
    let mut first = Acp::spawn(home.path(), &[], &[], &[]);
    first.initialize(&json!({}));
    let answer = first.ok(
        "session/new",
        &json!({"cwd": home.work, "mcpServers": [db_server()]}),
    );
    let session = answer["sessionId"].as_str().unwrap().to_owned();
    let mut second = Acp::spawn(home.path(), &[], &[], &[]);
    second.initialize(&json!({}));
    second.ok(
        "session/load",
        &json!({"sessionId": session, "cwd": home.work, "mcpServers": []}),
    );
    assert!(
        !second
            .agent_text(&session)
            .contains("Editor MCP tools changed"),
        "an empty list changes nothing"
    );
    let bound = home.rpc(
        "session.editor",
        &json!({"session_id": session, "mcp_servers": []}),
    );
    assert_eq!(bound["servers"], json!(["db"]));
    let mut third = Acp::spawn(home.path(), &[], &[], &[]);
    third.initialize(&json!({}));
    let other = json!({"name": "docs", "command": "true"});
    third.ok(
        "session/load",
        &json!({"sessionId": session, "cwd": home.work, "mcpServers": [other]}),
    );
    third.wait_for("the tool change notice", |f| {
        f["params"]["update"]["content"]["text"]
            == "Editor MCP tools changed; the new tool list applies from the next turn."
    });
}

/// Scenario: Forged cursor; the catalog holds only the profile's ACP
/// sessions with messages, newest first, a page at a time.
#[test]
fn the_session_catalog_pages_acp_sessions_and_refuses_a_forged_cursor() {
    let home = Home::new();
    let _daemon = daemon::start(home.path(), &[]);
    let mut acp = Acp::spawn(home.path(), &[], &[], &[]);
    acp.initialize(&json!({}));
    let empty = acp.new_session(&home.work);
    let mut used = Vec::new();
    for text in ["first", "second"] {
        let session = acp.new_session(&home.work);
        let id = acp.send_prompt(&session, text);
        acp.response(id);
        used.push(session);
    }
    common::ok(&common::run(home.path(), &["-z", "a cli session"]));
    let listed = acp.ok("session/list", &json!({}));
    let ids: Vec<&str> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["sessionId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [used[1].as_str(), used[0].as_str()]);
    assert!(!ids.contains(&empty.as_str()));
    assert!(listed["nextCursor"].is_null());
    let forged = acp.call("session/list", &json!({"cursor": "abc"}));
    assert_eq!(forged["error"]["code"], -32602);
    assert_eq!(forged["error"]["message"], "invalid cursor");
}

/// Scenario: Fork without servers.
#[test]
fn a_fork_without_servers_keeps_the_parents() {
    let home = Home::new();
    let _daemon = daemon::start(home.path(), &[]);
    let mut acp = Acp::spawn(home.path(), &[], &[], &[]);
    acp.initialize(&json!({}));
    let answer = acp.ok(
        "session/new",
        &json!({"cwd": home.work, "mcpServers": [db_server()]}),
    );
    let parent = answer["sessionId"].as_str().unwrap().to_owned();
    let id = acp.send_prompt(&parent, "something to fork");
    acp.response(id);
    let other = home.work.join("elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    let fork = acp.ok("session/fork", &json!({"sessionId": parent, "cwd": other}));
    let child = fork["sessionId"].as_str().unwrap().to_owned();
    assert_ne!(child, parent);
    let bound = home.rpc(
        "session.editor",
        &json!({"session_id": child, "mcp_servers": []}),
    );
    assert_eq!(bound["servers"], json!(["db"]));
    let settings = home.rpc("session.settings", &json!({"session_id": child}));
    assert_eq!(settings["cwd"], other.to_string_lossy().as_ref());
}

/// `session/resume` re-attaches like `session/load`; prompting a session
/// this process never attached is `not_found`.
#[test]
fn resume_reattaches_and_an_unattached_session_is_not_found() {
    let home = Home::new();
    let _daemon = daemon::start(home.path(), &[]);
    let mut acp = Acp::spawn(home.path(), &[], &[], &[]);
    acp.initialize(&json!({}));
    let session = acp.new_session(&home.work);
    let id = acp.send_prompt(&session, "remember me");
    acp.response(id);
    let mut other = Acp::spawn(home.path(), &[], &[], &[]);
    other.initialize(&json!({}));
    let refused = other.call(
        "session/prompt",
        &json!({"sessionId": session, "prompt": [{"type": "text", "text": "hi"}]}),
    );
    let message = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(message.starts_with("not_found"), "{refused}");
    other.ok(
        "session/resume",
        &json!({"sessionId": session, "cwd": home.work, "mcpServers": []}),
    );
    assert!(other.agent_text(&session).contains("mock: remember me"));
    let id = other.send_prompt(&session, "again");
    assert_eq!(other.response(id)["result"]["stopReason"], "end_turn");
}
