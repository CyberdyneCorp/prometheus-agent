//! A local fake of every hub service (tests only): GitHub's commit, tree,
//! raw-content, fork, ref, contents and pull endpoints over in-memory
//! repositories, plus static routes for the other services. Serve it with
//! [`FakeHub::start`] and point a hub at [`FakeHub::endpoints`] (the
//! binary reads `PMT_TEST_SKILLS_HUB` in debug builds).

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::http::Endpoints;

#[derive(Default)]
struct State {
    /// `owner/repo` -> current commit.
    heads: HashMap<String, String>,
    /// commit -> files.
    commits: HashMap<String, BTreeMap<String, Vec<u8>>>,
    /// path (with query) -> (status, body).
    routes: HashMap<String, (u16, Vec<u8>)>,
    /// `METHOD path` of every request, with its body.
    requests: Vec<(String, Vec<u8>)>,
}

/// The fake.
#[derive(Clone)]
pub struct FakeHub {
    url: String,
    state: Arc<Mutex<State>>,
}

impl FakeHub {
    /// Starts it on a loopback port.
    ///
    /// # Panics
    ///
    /// When it cannot listen.
    #[must_use]
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake hub");
        let url = format!("http://{}", listener.local_addr().expect("local addr"));
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = Arc::clone(&shared);
                std::thread::spawn(move || serve(stream, &state));
            }
        });
        Self { url, state }
    }

    /// Its base URL.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The endpoints of every service on it.
    #[must_use]
    pub fn endpoints(&self) -> Endpoints {
        Endpoints::under(&self.url)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Commits `files` as the new head of `owner/repo`; the commit SHA.
    #[allow(clippy::must_use_candidate)] // tests often ignore the SHA
    pub fn repo(&self, repo: &str, files: &[(&str, &[u8])]) -> String {
        let files: BTreeMap<String, Vec<u8>> = files
            .iter()
            .map(|(p, b)| ((*p).to_owned(), b.to_vec()))
            .collect();
        let mut hasher = Sha256::new();
        hasher.update(repo.as_bytes());
        for (path, bytes) in &files {
            hasher.update(path.as_bytes());
            hasher.update(bytes);
        }
        let sha = hasher
            .finalize()
            .iter()
            .take(20)
            .fold(String::new(), |mut sha, b| {
                let _ = write!(sha, "{b:02x}");
                sha
            });
        let mut state = self.lock();
        state.heads.insert(repo.to_owned(), sha.clone());
        state.commits.insert(sha.clone(), files);
        sha
    }

    /// Serves `body` with `status` at `path` (and query).
    pub fn route(&self, path: &str, status: u16, body: impl Into<Vec<u8>>) {
        self.lock()
            .routes
            .insert(path.to_owned(), (status, body.into()));
    }

    /// Serves `value` as JSON at `path`.
    pub fn json(&self, path: &str, value: &Value) {
        self.route(path, 200, value.to_string());
    }

    /// Every request so far: `METHOD /path` and its body.
    #[must_use]
    pub fn requests(&self) -> Vec<(String, Vec<u8>)> {
        self.lock().requests.clone()
    }
}

fn serve(stream: TcpStream, state: &Mutex<State>) {
    let Ok(clone) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(clone);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut parts = line.split_whitespace();
    let (method, target) = (
        parts.next().unwrap_or_default().to_owned(),
        parts.next().unwrap_or_default().to_owned(),
    );
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    let _ = reader.read_exact(&mut body);
    let (status, answer) = {
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .requests
            .push((format!("{method} {target}"), body.clone()));
        answer(&mut state, &method, &target, &body)
    };
    let mut stream = stream;
    let head = format!(
        "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        answer.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&answer);
}

fn json_answer(status: u16, value: &Value) -> (u16, Vec<u8>) {
    (status, value.to_string().into_bytes())
}

fn answer(state: &mut State, method: &str, target: &str, body: &[u8]) -> (u16, Vec<u8>) {
    if let Some(found) = state.routes.get(target) {
        return found.clone();
    }
    let path = target.split('?').next().unwrap_or(target);
    if let Some(rest) = path.strip_prefix("/raw/") {
        return raw(state, rest);
    }
    if let Some(rest) = path.strip_prefix("/github/repos/") {
        return github(state, method, rest, body);
    }
    (404, b"not found".to_vec())
}

fn raw(state: &State, rest: &str) -> (u16, Vec<u8>) {
    let mut parts = rest.splitn(4, '/');
    let (_, _, sha, file) = (
        parts.next(),
        parts.next(),
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    state
        .commits
        .get(sha)
        .and_then(|files| files.get(file))
        .map_or((404, b"not found".to_vec()), |b| (200, b.clone()))
}

fn github(state: &State, method: &str, rest: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let parts: Vec<&str> = rest.split('/').collect();
    let repo = parts.iter().take(2).copied().collect::<Vec<_>>().join("/");
    match (method, &parts[2.min(parts.len())..]) {
        ("GET", ["commits", at]) => {
            let head = state.heads.get(&repo);
            let sha = if *at == "HEAD" {
                head.cloned()
            } else {
                state.commits.contains_key(*at).then(|| (*at).to_owned())
            };
            sha.map_or((404, Vec::new()), |sha| {
                json_answer(200, &json!({"sha": sha}))
            })
        }
        ("GET", ["git", "trees", sha]) => {
            state.commits.get(*sha).map_or((404, Vec::new()), |files| {
                let tree: Vec<Value> = files
                    .keys()
                    .map(|p| json!({"path": p, "type": "blob"}))
                    .collect();
                json_answer(200, &json!({"sha": sha, "tree": tree, "truncated": false}))
            })
        }
        ("POST", ["forks"]) => {
            let name = parts.get(1).copied().unwrap_or_default();
            json_answer(202, &json!({"full_name": format!("publisher/{name}")}))
        }
        ("GET", []) => json_answer(200, &json!({"default_branch": "main"})),
        ("GET", ["git", "ref", "heads", _]) => {
            json_answer(200, &json!({"object": {"sha": "0123456789abcdef"}}))
        }
        ("POST", ["git", "refs"]) | ("PUT", ["contents", ..]) => {
            let echo: Value = serde_json::from_slice(body).unwrap_or_default();
            json_answer(201, &echo)
        }
        ("POST", ["pulls"]) => json_answer(
            201,
            &json!({"html_url": format!("https://github.com/{repo}/pull/1")}),
        ),
        _ => (404, Vec::new()),
    }
}
