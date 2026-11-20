//! `/codex-runtime on` in a running line-interface session (spec:
//! model-providers, Codex app-server runtime — Runtime switch deferred):
//! the session keeps answering through Prometheus's own `openai-codex`
//! route, and the next new session runs on the Codex app-server.

#![cfg(unix)]

mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::pty::{exit, prompt, spawn};
use common::{codex_fixture, ok, run, temp_home};
use expectrl::Expect;
use serde_json::{Value, json};

/// A ChatGPT backend answering every request with `answer <n>` on the
/// Responses wire; counts the requests.
fn backend() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&count);
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            common::read_http_request(&mut reader);
            // The quota probe (GET) is not a model call.
            if !request_line.starts_with("POST ") {
                let _ = write!(
                    stream,
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                continue;
            }
            let n = seen.fetch_add(1, Ordering::SeqCst);
            let body = format!(
                "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"answer {n}\"}}\n\n\
                 event: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"usage\":{{\"input_tokens\":5,\"output_tokens\":2}}}}}}\n\n"
            );
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (format!("http://{addr}/backend-api/codex"), count)
}

/// Signs `openai-codex` in from a Codex CLI login file.
fn sign_in(home: &Path) {
    let part = |v: &Value| URL_SAFE_NO_PAD.encode(v.to_string());
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 86_400;
    let claims = json!({"exp": exp, "https://api.openai.com/auth": {"chatgpt_account_id": "acct-1", "chatgpt_plan_type": "plus"}});
    let access = format!("{}.{}.sig", part(&json!({"alg": "none"})), part(&claims));
    let file = home.join("codex-auth.json");
    let tokens = json!({"access_token": access, "refresh_token": "rt-switch-0123456789"});
    std::fs::write(&file, json!({"tokens": tokens}).to_string()).unwrap();
    ok(&run(
        home,
        &[
            "auth",
            "import",
            "codex-cli",
            "--path",
            file.to_str().unwrap(),
        ],
    ));
}

#[test]
fn runtime_switch_is_deferred_to_the_next_session() {
    let home = temp_home();
    let (url, requests) = backend();
    let codex = codex_fixture(&home.path().join("bin"));
    ok(&run(home.path(), &["config", "path"]));
    std::fs::write(
        home.path().join("profiles/default/config.toml"),
        format!(
            "[model]\nprovider = \"openai-codex\"\ndefault = \"gpt-5\"\ncodex_bin = '{}'\n\n\
             [providers.openai-codex]\nbase_url = \"{url}\"\nkey_hosts = [\"127.0.0.1\"]\n\n\
             [budgets]\nallow_unpriced = true\n",
            codex.display()
        ),
    )
    .unwrap();
    sign_in(home.path());

    let mut p = spawn(home.path(), &["--no-color"]);
    prompt(&mut p);
    p.send_line("first").unwrap();
    p.expect("answer 0").unwrap();
    prompt(&mut p);
    p.send_line("/codex-runtime on").unwrap();
    p.expect("keeps its runtime").unwrap();
    prompt(&mut p);
    p.send_line("second").unwrap();
    p.expect("answer 1").unwrap();
    prompt(&mut p);
    exit(p);
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    let turns = |codex: &Path| {
        std::fs::read_to_string(codex.parent().unwrap().join("codex-fixture.log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains("\"turn/start\""))
            .count()
    };
    assert_eq!(turns(&codex), 0);

    // The next new session runs on the app-server.
    let out = ok(&run(home.path(), &["-z", "third"]));
    assert!(out.contains("codex saw: third"), "{out}");
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(turns(&codex), 1);
}
