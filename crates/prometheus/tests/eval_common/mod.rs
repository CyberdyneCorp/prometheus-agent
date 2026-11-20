//! What the eval tests share: suites written into a test home, JSON
//! output and a fake chat-completions provider that reports usage.

#![allow(dead_code, unreachable_pub)]

use std::io::{BufReader, Write as _};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;

use super::common::{ok, run};

/// One task of a test suite.
pub struct Task {
    id: String,
    prompt: String,
    mock: Option<String>,
    grader: String,
}

impl Task {
    /// Task `id` asking `prompt`; its mock model answers `answer` (none:
    /// the profile's provider answers) and an exact grader expects
    /// `expect`.
    pub fn new(id: &str, prompt: &str, answer: &str, expect: &str) -> Self {
        Self {
            id: id.to_owned(),
            prompt: prompt.to_owned(),
            mock: (!answer.is_empty())
                .then(|| format!("[[rule]]\nrepeat = true\ntext = {answer:?}\n")),
            grader: format!("type = \"exact\"\nexpect = {expect:?}\n"),
        }
    }

    /// The task's mock script instead.
    pub fn mock(mut self, script: &str) -> Self {
        self.mock = Some(script.to_owned());
        self
    }

    /// The task's grader instead.
    pub fn grader(mut self, toml: &str) -> Self {
        toml.clone_into(&mut self.grader);
        self
    }
}

/// Writes a suite of `tasks` at `dir` (`extra` goes into suite.toml).
pub fn suite(dir: &Path, extra: &str, tasks: &[Task]) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("suite.toml"),
        format!("name = \"test\"\nversion = \"1\"\ndomain = \"accounting\"\ndefault_timeout_secs = 60\n{extra}"),
    )
    .unwrap();
    for task in tasks {
        let t = dir.join("tasks").join(&task.id);
        std::fs::create_dir_all(t.join("grader")).unwrap();
        std::fs::write(t.join("task.toml"), format!("prompt = {:?}\n", task.prompt)).unwrap();
        std::fs::write(t.join("grader/grader.toml"), &task.grader).unwrap();
        if let Some(mock) = &task.mock {
            std::fs::write(t.join("mock.toml"), mock).unwrap();
        }
    }
}

/// `--json <args>`, parsed.
pub fn json(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend(args);
    serde_json::from_str(&ok(&run(home, &all))).unwrap()
}

/// `path` as a TOML string.
pub fn toml_path(path: &Path) -> String {
    toml::Value::String(path.to_string_lossy().into_owned()).to_string()
}

/// A JSON file.
pub fn read_json(path: &Path) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    )
    .unwrap()
}

/// A chat-completions server answering `text` and reporting a quarter of
/// the request body's bytes as input tokens and one output token.
pub struct FakeProvider {
    /// Its port.
    pub port: u16,
    calls: Arc<AtomicUsize>,
    bodies: Arc<std::sync::Mutex<Vec<String>>>,
}

impl FakeProvider {
    /// Starts it.
    pub fn start(text: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&bodies);
        let text = text.to_owned();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let body = super::common::read_http_request(&mut reader);
                counter.fetch_add(1, Ordering::SeqCst);
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&body).into_owned());
                let streamed = String::from_utf8_lossy(&body).contains("\"stream\":true");
                let prompt = body.len() / 4;
                let usage = format!(
                    "{{\"prompt_tokens\":{prompt},\"completion_tokens\":1,\"total_tokens\":{}}}",
                    prompt + 1
                );
                let response = if streamed {
                    let body = format!(
                        "data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":{text:?}}},\"finish_reason\":null}}]}}\n\n\
                         data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{usage}}}\n\n\
                         data: [DONE]\n\n"
                    );
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                } else {
                    let body = format!(
                        "{{\"id\":\"c1\",\"object\":\"chat.completion\",\"choices\":[{{\"index\":0,\"message\":{{\"role\":\"assistant\",\"content\":{text:?}}},\"finish_reason\":\"stop\"}}],\"usage\":{usage}}}"
                    );
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let mut stream = stream;
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            port,
            calls,
            bodies,
        }
    }

    /// Every request body so far.
    pub fn bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }

    /// Calls answered so far.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}
