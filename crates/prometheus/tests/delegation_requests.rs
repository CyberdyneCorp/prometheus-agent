//! What children send to their model (spec: subagent-delegation, Child
//! request overrides and direct endpoints): `delegation.request_overrides`
//! reach every child request body on whatever route the child uses, a
//! `delegation.base_url` sends children to a direct endpoint with their
//! own key, and an override the wire format lacks fails validation.

mod common;

use std::fmt::Write as _;
use std::io::{BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

use common::{append_config, command, mock_home, ok, run, run_with_stdin, stderr, temp_home};
use serde_json::{Value, json};

/// One request the fake endpoint received.
#[derive(Debug, Clone)]
struct Seen {
    authorization: Option<String>,
    body: Value,
}

impl Seen {
    fn is_child(&self) -> bool {
        self.body.to_string().contains("You are a subagent")
    }
}

fn sse(chunks: &[Value]) -> String {
    let mut out = String::new();
    for chunk in chunks {
        let _ = write!(out, "data: {chunk}\n\n");
    }
    out.push_str("data: [DONE]\n\n");
    out
}

/// The answer to `body`: a parent's first call delegates one task, a
/// child answers, a parent that got its tool result answers too.
fn answer(body: &Value) -> String {
    let text = body.to_string();
    let delta = |d: Value, finish: &str| json!({"choices": [{"index": 0, "delta": d, "finish_reason": finish}]});
    if text.contains("You are a subagent") {
        return sse(&[delta(json!({"content": "child done"}), "stop")]);
    }
    if text.contains("\"role\":\"tool\"") {
        return sse(&[delta(json!({"content": "parent done"}), "stop")]);
    }
    let call = json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
        "function": {"name": "delegate_task", "arguments": "{\"tasks\":[{\"goal\":\"child work\"}]}"}}]});
    sse(&[delta(call, "tool_calls")])
}

/// A Chat Completions endpoint recording every request.
fn endpoint() -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let record = Arc::clone(&seen);
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let (authorization, length) = headers(&mut reader);
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            let body: Value = serde_json::from_slice(&body).unwrap_or_default();
            let reply = answer(&body);
            record.lock().unwrap().push(Seen {
                authorization,
                body,
            });
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    (format!("http://{addr}/v1"), seen)
}

/// The `Authorization` header and the content length.
fn headers(reader: &mut impl std::io::BufRead) -> (Option<String>, usize) {
    let (mut authorization, mut length) = (None, 0);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            return (authorization, length);
        }
        let (name, value) = line.split_once(':').unwrap_or_default();
        match name.to_ascii_lowercase().as_str() {
            "authorization" => authorization = Some(value.trim().to_owned()),
            "content-length" => length = value.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
}

fn delegate(home: &Path, extra: &[&str]) -> std::process::Output {
    command(home)
        .current_dir(home)
        .args(["--json", "-z", "please delegate"])
        .args(extra)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

/// Scenario: Routing hint on every child call.
#[test]
fn a_childs_requests_carry_the_overrides_on_the_inherited_route() {
    let home = temp_home();
    let (url, seen) = endpoint();
    ok(&run(home.path(), &["config", "path"]));
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "OPENROUTER_API_KEY"],
        "sk-or-test-1234567890\n",
    ));
    std::fs::write(
        home.path().join("profiles/default/config.toml"),
        format!(
            "[model]\nprovider = \"openrouter\"\ndefault = \"vendor/parent-model\"\n\n\
             [providers.openrouter]\nbase_url = \"{url}\"\nkey_hosts = [\"127.0.0.1\"]\n\n\
             [budgets]\nallow_unpriced = true\n\n\
             [delegation.request_overrides]\ntemperature = 0.3\n\n\
             [delegation.request_overrides.extra_body.provider]\nsort = \"throughput\"\n"
        ),
    )
    .unwrap();
    let out = delegate(home.path(), &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let seen = seen.lock().unwrap().clone();
    let (children, parents): (Vec<&Seen>, Vec<&Seen>) = seen.iter().partition(|s| s.is_child());
    assert!(!children.is_empty() && parents.len() >= 2, "{seen:?}");
    for child in &children {
        assert_eq!(
            child.body["provider"]["sort"], "throughput",
            "{:?}",
            child.body
        );
        assert_eq!(child.body["temperature"], 0.3);
        assert_eq!(
            child.body["model"], "vendor/parent-model",
            "the route's own fields stay"
        );
        assert!(child.body["messages"].is_array() && child.body["stream"] == true);
    }
    for parent in &parents {
        assert!(
            parent.body.get("provider").is_none(),
            "only children: {:?}",
            parent.body
        );
    }
}

/// A direct endpoint: children call it with its own key and model, while
/// the parent stays on its route.
#[test]
fn children_call_a_direct_endpoint_with_its_own_key() {
    let home = mock_home();
    let (url, seen) = endpoint();
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "CHILD_ENDPOINT_KEY"],
        "sk-child-endpoint-42\n",
    ));
    append_config(
        home.path(),
        &format!(
            "\n[delegation]\nbase_url = \"{url}\"\napi_key_secret = \"CHILD_ENDPOINT_KEY\"\n\
             wire_format = \"chat_completions\"\nmodel = \"child-model\"\n\n\
             [budgets]\nallow_unpriced = true\n"
        ),
    );
    let script = home.path().join("parent.toml");
    std::fs::write(
        &script,
        "[[rule]]\nuser = \"please delegate\"\n[[rule.tool_calls]]\nname = \"delegate_task\"\n\
         arguments = { tasks = [{ goal = \"child work\" }] }\n\n\
         [[rule]]\ntool_result_regex = \"child done\"\ntext = \"the child answered\"\n\n\
         [[rule]]\nrepeat = true\ntext = \"no child answer\"\n",
    )
    .unwrap();
    let out = delegate(home.path(), &["--mock-script", script.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let answer: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        answer["answer"],
        "the child answered",
        "{:?}",
        common::active_messages(home.path())
    );
    let seen = seen.lock().unwrap().clone();
    assert!(
        !seen.is_empty() && seen.iter().all(Seen::is_child),
        "{seen:?}"
    );
    assert_eq!(seen[0].body["model"], "child-model");
    assert_eq!(
        seen[0].authorization.as_deref(),
        Some("Bearer sk-child-endpoint-42")
    );
}

/// An override the wire format does not take fails validation (exit 78).
#[test]
fn an_override_the_wire_format_lacks_fails_validation() {
    let home = mock_home();
    append_config(
        home.path(),
        "\n[delegation]\nbase_url = \"https://models.example.com/anthropic\"\n\n\
         [delegation.request_overrides]\nreasoning_effort = \"high\"\n",
    );
    let out = run(home.path(), &["config", "validate"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("delegation.request_overrides.reasoning_effort"),
        "{}",
        stderr(&out)
    );
}
