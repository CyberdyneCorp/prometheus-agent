//! The Responses endpoint, idempotent admission, memory scoping by the
//! caller's session key, and an OpenAI SDK client against the daemon's
//! API server (spec: api-server, Responses endpoint and response store;
//! Idempotent admission for Chat Completions and Responses; Caller
//! instructions and long-term memory scoping; Model listing and
//! `Open WebUI` compatibility).

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;

use serde_json::{Value, json};

use self::common::api::{self, Request, chat, key_of};
use self::common::{append_config, daemon, ok, run};

fn script(home: &Path, rules: &str) {
    let path = home.join("api-script.toml");
    std::fs::write(&path, rules).unwrap();
    append_config(
        home,
        &format!(
            "\n[providers.mock]\nscript = {:?}\n",
            path.to_string_lossy()
        ),
    );
}

fn respond(port: u16, body: &Value) -> api::Answer {
    Request::new("POST", "/v1/responses")
        .auth("default")
        .json(body)
        .send(port)
}

fn kinds(response: &Value) -> Vec<String> {
    response["output"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["type"].as_str().unwrap().to_owned())
        .collect()
}

/// Scenario: Chained turn.
#[test]
fn a_chained_response_sees_the_earlier_tool_call_and_lists_only_its_own_items() {
    let home = api::home("");
    script(
        home.path(),
        "[[rule]]\nuser = \"README\"\nstep = 2\ntext = \"saw the earlier listing\"\nrepeat = true\n\
         [[rule]]\nuser = \"list\"\nstep = 0\n[[rule.tool_calls]]\nname = \"terminal\"\narguments = { command = \"ls\" }\n\
         [[rule]]\ntool_result_regex = \"[\\\\s\\\\S]*\"\ntext = \"listed\"\nrepeat = true\n\
         [[rule]]\ntext = \"other\"\nrepeat = true\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let first = respond(port, &json!({"input": "list the files"}));
    assert_eq!(first.status, 200, "{}", first.body);
    let first = first.json();
    assert_eq!(first["status"], "completed");
    assert_eq!(
        kinds(&first),
        ["function_call", "function_call_output", "message"],
        "{first}"
    );
    assert_eq!(first["output"][0]["status"], "completed");
    let id = first["id"].as_str().unwrap();
    let second = respond(
        port,
        &json!({"input": "Now show me the README", "previous_response_id": id}),
    )
    .json();
    assert_eq!(kinds(&second), ["message"], "{second}");
    assert_eq!(
        second["output"][0]["content"][0]["text"],
        "saw the earlier listing"
    );
    assert_eq!(
        second["prometheus"]["session_id"],
        first["prometheus"]["session_id"]
    );
    let stored = Request::new("GET", &format!("/v1/responses/{id}"))
        .auth("default")
        .send(port);
    assert_eq!(stored.json()["id"], id);
    let deleted = Request::new("DELETE", &format!("/v1/responses/{id}"))
        .auth("default")
        .send(port);
    assert_eq!(deleted.json()["deleted"], true);
    let gone = Request::new("GET", &format!("/v1/responses/{id}"))
        .auth("default")
        .send(port);
    assert_eq!(gone.status, 404);
}

/// Scenario: Echoed reasoning items ignored on input.
#[test]
fn echoed_reasoning_items_are_not_user_turns() {
    let home = api::home("");
    script(
        home.path(),
        "[[rule]]\nreasoning = \"thinking it over\"\ntext = \"first answer\"\nstep = 0\n\
         [[rule]]\nuser = \"follow up\"\ntext = \"second answer\"\nrepeat = true\n\
         [[rule]]\ntext = \"echo treated as a turn\"\nrepeat = true\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let first = respond(port, &json!({"input": "start"})).json();
    assert_eq!(kinds(&first), ["reasoning", "message"], "{first}");
    let mut input = first["output"].as_array().unwrap().clone();
    input.push(json!({"role": "user", "content": "follow up"}));
    let second = respond(port, &json!({"input": input})).json();
    assert_eq!(
        second["output"][0]["content"][0]["text"], "second answer",
        "{second}"
    );
    let session = second["prometheus"]["session_id"].as_str().unwrap();
    let users: Vec<String> = common::session_messages(home.path(), session)
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(users, ["follow up"], "only the user message is a user turn");
}

/// Scenario: Retry after network loss. Scenario: Key reused for
/// different work.
#[test]
fn a_retried_request_replays_the_original_answer_and_runs_once() {
    let home = api::home("");
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let send = |body: &Value| {
        Request::new("POST", "/v1/chat/completions")
            .auth("default")
            .header("Idempotency-Key", "k1")
            .json(body)
            .send(port)
    };
    let body = json!({"messages": [{"role": "user", "content": "do it once"}]});
    let first = send(&body);
    assert_eq!(first.status, 200);
    let session = first.header("x-prometheus-session-id").unwrap().to_owned();
    let mut streamed = body.clone();
    streamed["stream"] = true.into();
    let retry = send(&streamed);
    assert_eq!(retry.header("idempotency-replayed"), Some("true"));
    let text: String = retry
        .events()
        .iter()
        .filter(|(_, _, data)| data != "[DONE]")
        .filter_map(|(_, _, data)| serde_json::from_str::<Value>(data).ok())
        .filter_map(|c| {
            c["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(text, "mock: do it once");
    assert_eq!(retry.events().last().unwrap().2, "[DONE]");
    let users = common::session_messages(home.path(), &session)
        .iter()
        .filter(|m| m["role"] == "user")
        .count();
    assert_eq!(users, 1, "one turn ran");
    let other = send(&json!({"messages": [{"role": "user", "content": "something else"}]}));
    assert_eq!(other.status, 409);
    assert_eq!(other.json()["error"]["code"], "admission_conflict");
    let bad = Request::new("POST", "/v1/responses")
        .auth("default")
        .header("Idempotency-Key", "has space")
        .json(&json!({"input": "x"}))
        .send(port);
    assert_eq!(bad.status, 400);
    // Responses keys are their own namespace and replay too.
    let once = |input: &str| {
        Request::new("POST", "/v1/responses")
            .auth("default")
            .header("Idempotency-Key", "k1")
            .json(&json!({"input": input}))
            .send(port)
    };
    let response = once("respond once").json();
    let again = once("respond once");
    assert_eq!(again.json()["id"], response["id"]);
    assert_eq!(once("respond twice").status, 409);
}

/// Places the testing memory provider plugin in the default profile.
fn memory_provider(home: &Path) {
    let dir = home.join("profiles/default/plugins/testing-memory");
    pmt_plugins::testkit::install("testing-memory", &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        "manifest_version = 1\nid = \"testing-memory\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
         description = \"test\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
         worlds = [\"memory-provider@1.0\"]\n\n[capabilities]\n",
    )
    .unwrap();
    pmt_plugins::record::InstallRecord {
        source: pmt_plugins::record::Origin::Adopted,
        location: dir.to_string_lossy().into_owned(),
        r#ref: None,
        package_digest: None,
        digest: pmt_plugins::digest::of_dir(&dir).unwrap(),
        signer: None,
        tier: None,
        installed_at: "2026-11-08T00:00:00Z".to_owned(),
        flags: pmt_plugins::record::Flags::default(),
        held_update: None,
    }
    .write(&dir)
    .unwrap();
    ok(&run(
        home,
        &["plugins", "enable", "testing-memory", "--grant", "all"],
    ));
    append_config(home, "\n[memory]\nprovider = \"testing-memory\"\n");
}

/// Scenario: Session key scopes memory.
#[test]
fn the_session_key_is_the_memory_scope_across_chats() {
    let home = api::home("");
    memory_provider(home.path());
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let mut sessions = Vec::new();
    for opening in ["chat one", "chat two"] {
        let answer = Request::new("POST", "/v1/chat/completions")
            .auth("default")
            .header("X-Prometheus-Session-Key", "webui:user-42")
            .json(&json!({"messages": [{"role": "user", "content": opening}]}))
            .send(port);
        assert_eq!(answer.status, 200, "{}", answer.body);
        assert_eq!(
            answer.header("x-prometheus-session-key"),
            Some("webui:user-42")
        );
        sessions.push(answer.header("x-prometheus-session-id").unwrap().to_owned());
    }
    assert_ne!(sessions[0], sessions[1]);
    let users = pmt_plugins::host::state::StateDir::new(
        &home.path().join("profiles/default"),
        "testing-memory",
    )
    .get("users")
    .unwrap()
    .map(|b| String::from_utf8_lossy(&b).into_owned())
    .unwrap_or_default();
    for session in &sessions {
        assert!(
            users.contains(&format!("{session} webui:user-42")),
            "{users}"
        );
    }
    let bad = Request::new("POST", "/v1/chat/completions")
        .auth("default")
        .header("X-Prometheus-Session-Key", "a\u{7}b")
        .json(&json!({"messages": [{"role": "user", "content": "x"}]}))
        .send(port);
    assert_eq!(bad.status, 400);
    // On Responses the key also names the conversation to continue.
    let first = Request::new("POST", "/v1/responses")
        .auth("default")
        .header("X-Prometheus-Session-Key", "webui:user-7")
        .json(&json!({"input": "remember me"}))
        .send(port)
        .json();
    let next = Request::new("POST", "/v1/responses")
        .auth("default")
        .header("X-Prometheus-Session-Key", "webui:user-7")
        .json(&json!({"input": "again"}))
        .send(port)
        .json();
    assert_eq!(
        next["prometheus"]["session_id"],
        first["prometheus"]["session_id"]
    );
}

/// The OpenAI Rust SDK as a client: model listing, a chat completion and
/// a streamed one, and a Responses call.
#[test]
fn an_openai_sdk_client_completes_chats_and_responses() {
    use async_openai::Client;
    use async_openai::config::OpenAIConfig;
    use async_openai::types::chat::{
        ChatCompletionRequestUserMessageArgs, CreateChatCompletionRequestArgs,
    };
    use futures_util::StreamExt as _;

    let home = api::home("");
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    let config = OpenAIConfig::new()
        .with_api_base(format!("http://127.0.0.1:{port}/v1"))
        .with_api_key(key_of("default"));
    // The workspace's reqwest carries rustls without a default crypto
    // provider; pmt-net's client brings ring.
    let http = pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap();
    let client = Client::build(http, config);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let models = client.models().list().await.unwrap();
        assert_eq!(models.data[0].id, "prometheus-agent");
        let request = CreateChatCompletionRequestArgs::default()
            .model("prometheus-agent")
            .messages([ChatCompletionRequestUserMessageArgs::default()
                .content("hello sdk")
                .build()
                .unwrap()
                .into()])
            .build()
            .unwrap();
        let answer = client.chat().create(request.clone()).await.unwrap();
        assert_eq!(
            answer.choices[0].message.content.as_deref(),
            Some("mock: hello sdk")
        );
        // With the SDK's `byot` feature the caller sets `stream` itself.
        let mut streaming = request;
        streaming.stream = Some(true);
        let mut stream = client.chat().create_stream(streaming).await.unwrap();
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.unwrap();
            if let Some(choice) = chunk.choices.first()
                && let Some(delta) = &choice.delta.content
            {
                text.push_str(delta);
            }
        }
        assert_eq!(text, "mock: hello sdk");
        let response: Value = client
            .responses()
            .create_byot(json!({"model": "prometheus-agent", "input": "hello responses"}))
            .await
            .unwrap();
        assert_eq!(
            response["output"][0]["content"][0]["text"],
            "mock: hello responses"
        );
    });
    let _ = chat(
        port,
        "default",
        &json!({"messages": [{"role": "user", "content": "done"}]}),
    );
}
