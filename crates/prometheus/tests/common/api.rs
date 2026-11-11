//! The API server of a test daemon: a home whose profiles enable it with
//! their own keys, and a minimal HTTP/1.1 client (plain responses and
//! server-sent event streams).

use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use super::{append_config, mock_home, ok, run, run_with_stdin};

/// The key of profile `name`.
pub fn key_of(profile: &str) -> String {
    format!("test-key-{profile}-0123456789abcdefghijklmnopqrstuvwxyz")
}

/// A free loopback port (bound and released).
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Enables the API server in `profile` (with its key) and appends `extra`
/// to its configuration.
pub fn enable(home: &Path, profile: &str, extra: &str) {
    let config = home.join("profiles").join(profile).join("config.toml");
    let text = std::fs::read_to_string(&config).unwrap_or_default();
    let text = format!("{text}\n[api_server]\nenabled = true\n{extra}\n");
    std::fs::write(&config, text).unwrap();
    ok(&run_with_stdin(
        home,
        &["-p", profile, "secrets", "set", "API_SERVER_KEY"],
        &format!("{}\n", key_of(profile)),
    ));
}

/// A mock home whose default profile serves the API with `extra` config;
/// the listener port is returned with it.
pub fn home(extra: &str) -> (tempfile::TempDir, u16) {
    let home = mock_home();
    enable(home.path(), "default", extra);
    let port = listen(home.path(), "");
    (home, port)
}

/// Writes `daemon.toml` with the API listener on a free port and `extra`.
pub fn listen(home: &Path, extra: &str) -> u16 {
    let port = free_port();
    std::fs::write(
        home.join("daemon.toml"),
        format!("[api_server]\nlisten = \"127.0.0.1:{port}\"\n{extra}\n"),
    )
    .unwrap();
    port
}

/// Creates profile `name` (mock provider) served by the same daemon.
pub fn profile(home: &Path, name: &str, extra: &str) {
    ok(&run(home, &["profile", "create", name, "--no-alias"]));
    for (key, value) in [("model.provider", "mock"), ("model.default", "mock")] {
        ok(&run(home, &["-p", name, "config", "set", key, value]));
    }
    enable(home, name, extra);
}

/// Serves every profile from the default profile's daemon.
pub fn multiplex(home: &Path) {
    append_config(home, "\n[gateway]\nmultiplex = true\n");
}

/// An answer.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Answer {
    /// The body as JSON.
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }

    /// A header's value.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The server-sent events of the body: `(id, event, data)`.
    pub fn events(&self) -> Vec<(Option<String>, Option<String>, String)> {
        parse_events(&self.body)
    }
}

/// Parses an event stream.
pub fn parse_events(text: &str) -> Vec<(Option<String>, Option<String>, String)> {
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let (mut id, mut event, mut data) = (None, None, Vec::new());
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("id: ") {
                id = Some(v.to_owned());
            } else if let Some(v) = line.strip_prefix("event: ") {
                event = Some(v.to_owned());
            } else if let Some(v) = line.strip_prefix("data: ") {
                data.push(v);
            }
        }
        if !data.is_empty() {
            out.push((id, event, data.join("\n")));
        }
    }
    out
}

/// A request to the API server on `port`.
pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

impl<'a> Request<'a> {
    pub fn new(method: &'a str, path: &'a str) -> Self {
        Self {
            method,
            path,
            headers: Vec::new(),
            body: None,
        }
    }

    /// With profile `profile`'s key.
    pub fn auth(self, profile: &str) -> Self {
        self.header("Authorization", &format!("Bearer {}", key_of(profile)))
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn json(mut self, body: &Value) -> Self {
        self.body = Some(body.to_string());
        self.header("Content-Type", "application/json")
    }

    fn head(&self, port: u16) -> String {
        let mut head = format!("{} {} HTTP/1.1\r\n", self.method, self.path);
        if !self
            .headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("host"))
        {
            let _ = write!(head, "Host: 127.0.0.1:{port}\r\n");
        }
        for (name, value) in &self.headers {
            let _ = write!(head, "{name}: {value}\r\n");
        }
        let body = self.body.as_deref().unwrap_or_default();
        let _ = write!(
            head,
            "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        head
    }

    /// Sends it and reads the whole answer.
    pub fn send(&self, port: u16) -> Answer {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_mins(2)))
            .unwrap();
        stream.write_all(self.head(port).as_bytes()).unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        parse(&raw)
    }

    /// Sends it and reads the answer's event stream line by line until
    /// `stop` returns true for an event; the connection is then dropped.
    pub fn stream_until(
        &self,
        port: u16,
        mut stop: impl FnMut(&(Option<String>, Option<String>, String)) -> bool,
    ) -> Vec<(Option<String>, Option<String>, String)> {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_mins(2)))
            .unwrap();
        stream.write_all(self.head(port).as_bytes()).unwrap();
        let mut reader = BufReader::new(stream);
        let mut block = String::new();
        let mut seen = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap() == 0 {
                return seen;
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.starts_with("id: ")
                || trimmed.starts_with("event: ")
                || trimmed.starts_with("data: ")
            {
                block.push_str(trimmed);
                block.push('\n');
            } else if trimmed.is_empty() && !block.is_empty() {
                let mut events = parse_events(&block);
                block.clear();
                if let Some(event) = events.pop() {
                    let done = stop(&event);
                    seen.push(event);
                    if done {
                        return seen;
                    }
                }
            }
        }
    }
}

fn parse(raw: &[u8]) -> Answer {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(": "))
        .map(|(n, v)| (n.to_owned(), v.to_owned()))
        .collect();
    let chunked = headers
        .iter()
        .any(|(n, v)| n.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked"));
    let body = if chunked {
        dechunk(body)
    } else {
        body.to_owned()
    };
    Answer {
        status,
        headers,
        body,
    }
}

fn dechunk(mut rest: &str) -> String {
    let mut out = String::new();
    while let Some((size, after)) = rest.split_once("\r\n") {
        let Ok(size) = usize::from_str_radix(size.trim(), 16) else {
            break;
        };
        if size == 0 || after.len() < size {
            break;
        }
        out.push_str(&after[..size]);
        rest = after[size..].trim_start_matches("\r\n");
    }
    out
}

/// `POST /v1/chat/completions` of `profile` with `body`.
pub fn chat(port: u16, profile: &str, body: &Value) -> Answer {
    Request::new("POST", "/v1/chat/completions")
        .auth(profile)
        .json(body)
        .send(port)
}

/// Polls run `id` of `profile` until it is terminal; its last state.
pub fn run_until_terminal(port: u16, profile: &str, id: &str) -> Value {
    let prefix = if profile == "default" {
        String::new()
    } else {
        format!("/p/{profile}")
    };
    let deadline = std::time::Instant::now() + Duration::from_mins(2);
    loop {
        let run = Request::new("GET", &format!("{prefix}/v1/runs/{id}"))
            .auth(profile)
            .send(port)
            .json();
        if ["completed", "failed", "cancelled", "interrupted"]
            .contains(&run["status"].as_str().unwrap_or(""))
        {
            return run;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "run never ended: {run}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Polls run `id` until `ready` holds for it.
pub fn run_until(port: u16, profile: &str, id: &str, ready: impl Fn(&Value) -> bool) -> Value {
    let prefix = if profile == "default" {
        String::new()
    } else {
        format!("/p/{profile}")
    };
    let deadline = std::time::Instant::now() + Duration::from_mins(2);
    loop {
        let run = Request::new("GET", &format!("{prefix}/v1/runs/{id}"))
            .auth(profile)
            .send(port)
            .json();
        if ready(&run) {
            return run;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "run never got there: {run}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
