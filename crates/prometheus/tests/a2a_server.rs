//! The A2A server of a running daemon (spec: agent-interop, tasks 74.1 and
//! 74.2): enablement and binding, the Agent Card, the JSON-RPC methods and
//! task lifecycle, conversation identity, authentication, trust, rate
//! limits, inbound safety and push notifications.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;

use serde_json::{Value, json};

use self::common::a2a::{self, message, rpc, token_of};
use self::common::api::Request;
use self::common::{append_config, daemon, mock_home, run, stderr};

/// Points the default profile's mock provider at a script with `rules`.
fn script(home: &Path, rules: &str) {
    let path = home.join("a2a-script.toml");
    std::fs::write(&path, rules).unwrap();
    append_config(
        home,
        &format!(
            "\n[providers.mock]\nscript = {:?}\n",
            path.to_string_lossy()
        ),
    );
}

fn a2a_sessions(home: &Path) -> Vec<Value> {
    let listed = common::ok(&run(
        home,
        &["--json", "sessions", "list", "--all", "--limit", "100"],
    ));
    let listed: Value = serde_json::from_str(&listed).unwrap();
    listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["title"].as_str().is_some_and(|t| t.starts_with("A2A ")))
        .cloned()
        .collect()
}

fn task(answer: &common::api::Answer) -> Value {
    let body = answer.json();
    assert!(body["error"].is_null(), "{}", answer.body);
    body["result"]["task"].clone()
}

/// Scenario: Wildcard bind without tokens.
#[test]
fn a_wildcard_bind_without_opt_in_fails_validation() {
    let home = mock_home();
    std::fs::write(
        home.path().join("daemon.toml"),
        "[a2a]\nlisten = \"0.0.0.0:9900\"\n",
    )
    .unwrap();
    let out = run(home.path(), &["daemon", "run"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("non-loopback a2a.listen requires a2a.public = true and peer tokens"),
        "{}",
        stderr(&out)
    );
}

/// Scenario: Browser page refused. Scenario: Card behind a reverse proxy.
#[test]
fn a_tokenless_server_refuses_browsers_and_cards_name_the_public_url() {
    let home = mock_home();
    a2a::listen(home.path());
    a2a::enable(
        home.path(),
        &[],
        &["local"],
        "public_url = \"https://agent.example.com/a2a\"\nadvertised_toolsets = [\"todo\"]",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let port = a2a::port(home.path());
    let page = Request::new("POST", "/")
        .header("Origin", "https://evil.example")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "SendMessage", "params": message("hi", None)}))
        .send(port);
    assert_eq!(page.status, 403, "{}", page.body);
    assert_eq!(page.json()["error"], "browser-originated request refused");
    let rebound = Request::new("GET", "/.well-known/agent-card.json")
        .header("Host", "attacker.example")
        .send(port);
    assert_eq!(rebound.status, 403);
    for path in ["/.well-known/agent-card.json", "/.well-known/agent.json"] {
        let card = Request::new("GET", path).send(port);
        assert_eq!(card.status, 200, "{}", card.body);
        let card = card.json();
        assert_eq!(
            card["supportedInterfaces"][0]["url"],
            "https://agent.example.com/a2a"
        );
        assert_eq!(card["url"], "https://agent.example.com/a2a");
        assert_eq!(card["capabilities"]["streaming"], true);
        assert_eq!(card["capabilities"]["pushNotifications"], true);
        let skills: Vec<&str> = card["skills"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(skills, ["todo"], "{card}");
        assert!(card["name"].as_str().unwrap().ends_with("-default"));
    }
    let unknown = rpc(port, None, "tasks/explode", &json!({}));
    assert_eq!(unknown.json()["error"]["code"], -32601);
}

/// Scenario: Same context id from two peers. Scenario: Untrusted identity.
#[test]
fn peers_get_their_own_sessions_and_untrusted_peers_get_no_turn() {
    let home = mock_home();
    script(home.path(), "[[rule]]\ntext = \"pong\"\nrepeat = true\n");
    a2a::listen(home.path());
    a2a::enable(
        home.path(),
        &["alice", "bob", "carol"],
        &["alice", "bob"],
        "",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let port = a2a::port(home.path());
    let bad = Request::new("POST", "/")
        .header("Authorization", "Bearer wrong")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "GetTask", "params": {"id": "x"}}))
        .send(port);
    assert_eq!(bad.status, 401);
    let mut sessions = Vec::new();
    for peer in ["alice", "bob", "alice"] {
        let answer = rpc(
            port,
            Some(peer),
            "SendMessage",
            &message("ping", Some("ctx-1")),
        );
        let task = task(&answer);
        assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED", "{task}");
        assert_eq!(task["artifacts"][0]["parts"][0]["text"], "pong");
        assert_eq!(task["contextId"], "ctx-1");
        sessions.push(a2a_sessions(home.path()).len());
    }
    // alice and bob each got a session; alice's second message reused hers.
    assert_eq!(sessions, [1, 2, 2]);
    let carol = rpc(
        port,
        Some("carol"),
        "SendMessage",
        &message("ping", Some("ctx-1")),
    );
    let carol = task(&carol);
    assert_eq!(carol["status"]["state"], "TASK_STATE_REJECTED");
    assert_eq!(
        carol["status"]["message"]["parts"][0]["text"],
        "untrusted_peer"
    );
    assert_eq!(a2a_sessions(home.path()).len(), 2, "carol started no turn");
    // A caller sees only its own tasks.
    let id = carol["id"].as_str().unwrap();
    let foreign = rpc(port, Some("alice"), "GetTask", &json!({"id": id}));
    assert_eq!(foreign.json()["error"]["code"], -32001);
    let own = rpc(port, Some("carol"), "tasks/get", &json!({"id": id}));
    assert_eq!(own.json()["result"]["status"]["state"], "rejected");
    let audit = std::fs::read_to_string(
        home.path()
            .join("profiles/default/logs/security-audit.jsonl"),
    )
    .unwrap();
    assert!(
        audit.contains("\"kind\":\"a2a\"") && audit.contains("alice/ctx-1/"),
        "{audit}"
    );
    assert!(
        !audit.contains("ping"),
        "content is not audited by default: {audit}"
    );
}

/// Scenario: Slash command from a peer. Scenario: Ping-pong capped.
#[test]
fn peer_text_is_never_a_command_and_contexts_are_capped() {
    let home = mock_home();
    script(
        home.path(),
        "[[rule]]\nuser = \"/model gpt-5\"\ntext = \"read it as text\"\nrepeat = true\n\
         [[rule]]\ntext = \"ok\"\nrepeat = true\n",
    );
    a2a::listen(home.path());
    a2a::enable(
        home.path(),
        &["alice"],
        &["alice"],
        "rate_limit_per_minute = 100",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let port = a2a::port(home.path());
    let answer = rpc(
        port,
        Some("alice"),
        "SendMessage",
        &message("/model gpt-5", Some("c1")),
    );
    let task = task(&answer);
    assert_eq!(task["artifacts"][0]["parts"][0]["text"], "read it as text");
    let session = &a2a_sessions(home.path())[0];
    assert_eq!(session["model"], "mock", "{session}");
    for _ in 0..4 {
        let answer = rpc(
            port,
            Some("alice"),
            "SendMessage",
            &message("again", Some("c1")),
        );
        assert_eq!(
            self::task(&answer)["status"]["state"],
            "TASK_STATE_COMPLETED"
        );
    }
    let sixth = rpc(
        port,
        Some("alice"),
        "SendMessage",
        &message("again", Some("c1")),
    );
    let sixth = self::task(&sixth);
    assert_eq!(sixth["status"]["state"], "TASK_STATE_REJECTED");
    assert_eq!(
        sixth["status"]["message"]["parts"][0]["text"],
        "pingpong_cap"
    );
    let other = rpc(
        port,
        Some("alice"),
        "SendMessage",
        &message("again", Some("c2")),
    );
    assert_eq!(
        self::task(&other)["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
}

/// Scenario: Push to a private address.
#[test]
fn push_urls_obey_the_egress_policy_and_rates_are_limited() {
    let home = mock_home();
    a2a::listen(home.path());
    a2a::enable(
        home.path(),
        &["alice"],
        &["alice"],
        "rate_limit_per_minute = 3",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let port = a2a::port(home.path());
    let mut params = message("hi", None);
    params["configuration"] =
        json!({"taskPushNotificationConfig": {"url": "http://169.254.169.254/hook"}});
    let refused = rpc(port, Some("alice"), "SendMessage", &params);
    let error = &refused.json()["error"];
    assert_eq!(error["code"], -32602, "{}", refused.body);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("refused by the egress policy"),
        "{error}"
    );
    let task = task(&rpc(
        port,
        Some("alice"),
        "SendMessage",
        &message("hi", None),
    ));
    let create = rpc(
        port,
        Some("alice"),
        "CreateTaskPushNotificationConfig",
        &json!({"taskId": task["id"], "url": "http://169.254.169.254/hook"}),
    );
    assert!(
        create.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("egress")
    );
    let limited = rpc(port, Some("alice"), "GetTask", &json!({"id": task["id"]}));
    assert_eq!(limited.status, 429, "{}", limited.body);
    assert_eq!(limited.json()["error"]["code"], -32029);
}

/// Pushes are signed over the timestamp and body with the peer's token.
#[test]
fn pushes_are_signed_with_the_peer_token() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let receiver = runtime.block_on(wiremock::MockServer::start());
    runtime.block_on(
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&receiver),
    );
    let home = mock_home();
    append_config(home.path(), "\n[security.egress]\nallow_private = true\n");
    a2a::listen(home.path());
    a2a::enable(home.path(), &["alice"], &["alice"], "");
    let _daemon = daemon::start(home.path(), &[]);
    let port = a2a::port(home.path());
    let mut params = message("hi", None);
    params["configuration"] = json!({"taskPushNotificationConfig": {"url": format!("{}/hook", receiver.uri()), "token": "n1"}});
    let done = task(&rpc(port, Some("alice"), "SendMessage", &params));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let requests = loop {
        let requests = runtime
            .block_on(receiver.received_requests())
            .unwrap_or_default();
        if requests
            .iter()
            .any(|r| String::from_utf8_lossy(&r.body).contains("COMPLETED"))
        {
            break requests;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no completed push: {requests:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let last = requests.last().unwrap();
    let header = |name: &str| last.headers.get(name).unwrap().to_str().unwrap().to_owned();
    let timestamp: i64 = header("x-a2a-timestamp").parse().unwrap();
    assert!(pmt_a2a_verify(
        &token_of("alice"),
        timestamp,
        &last.body,
        &header("x-a2a-signature")
    ));
    assert_eq!(header("x-a2a-notification-token"), "n1");
    let body: Value = serde_json::from_slice(&last.body).unwrap();
    assert_eq!(body["statusUpdate"]["taskId"], done["id"]);
}

/// HMAC-SHA256 over `<timestamp>.<body>`, as the receiver checks it.
fn pmt_a2a_verify(secret: &str, timestamp: i64, body: &[u8], given: &str) -> bool {
    use std::fmt::Write as _;
    let mut message = format!("{timestamp}.").into_bytes();
    message.extend_from_slice(body);
    let mac = ring::hmac::sign(
        &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes()),
        &message,
    );
    let hex = mac.as_ref().iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    });
    given == format!("sha256={hex}")
}

/// Streaming sends JSON-RPC-enveloped SSE frames ending with the final
/// status, and the pre-1.0 aliases answer in their own shapes.
#[test]
fn streams_and_pre_1_0_aliases_work() {
    let home = mock_home();
    script(
        home.path(),
        "[[rule]]\ntext = \"streamed\"\nrepeat = true\n",
    );
    a2a::listen(home.path());
    a2a::enable(home.path(), &["alice"], &["alice"], "");
    let _daemon = daemon::start(home.path(), &[]);
    let port = a2a::port(home.path());
    let body = json!({"jsonrpc": "2.0", "id": "s1", "method": "SendStreamingMessage", "params": message("go", None)});
    let events = Request::new("POST", "/")
        .header("Authorization", &format!("Bearer {}", token_of("alice")))
        .json(&body)
        .stream_until(port, |(_, _, data)| data.contains("TASK_STATE_COMPLETED"));
    let frames: Vec<Value> = events
        .iter()
        .map(|(_, _, d)| serde_json::from_str(d).unwrap())
        .collect();
    assert!(
        frames
            .iter()
            .all(|f| f["jsonrpc"] == "2.0" && f["id"] == "s1"),
        "{frames:?}"
    );
    assert!(frames[0]["result"]["task"].is_object(), "{frames:?}");
    assert!(
        frames
            .iter()
            .any(|f| f["result"]["artifactUpdate"]["artifact"]["parts"][0]["text"] == "streamed")
    );
    let legacy = json!({"message": {"kind": "message", "messageId": "m1", "role": "user", "parts": [{"kind": "text", "text": "old"}]}});
    let answer = rpc(port, Some("alice"), "message/send", &legacy).json();
    assert_eq!(answer["result"]["kind"], "task", "{answer}");
    assert_eq!(answer["result"]["status"]["state"], "completed");
    let id = answer["result"]["id"].clone();
    let listed = rpc(port, Some("alice"), "ListTasks", &json!({})).json();
    assert_eq!(listed["result"]["totalSize"], 2, "{listed}");
    let cancel = rpc(port, Some("alice"), "CancelTask", &json!({"id": id})).json();
    assert_eq!(
        cancel["error"]["code"], -32002,
        "a finished task is not cancelable"
    );
}
