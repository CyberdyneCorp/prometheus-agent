//! Catalog adapter plugins under test (spec: platform-adapters, Catalog
//! adapter set): a first-party plugin of `plugins/first-party/<name>`
//! built for wasm32 and installed in a throwaway home through
//! [`PluginKit`], a mock of its platform's HTTP API ([`MockHttp`]), a raw
//! HTTP client for posting the platform's webhooks to the adapter
//! ([`post`]) and the socket bridge sidecar run with the system Node
//! ([`Bridge`]).

#![allow(dead_code)]

use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use pmt_gateway::messaging::conformance::{MockPlatform, Report};
use prometheus::plugins_cmd::adapter_plugin::PluginKit;
use serde_json::{Value, json};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A scratch directory under the workspace's `target/`.
pub fn scratch() -> tempfile::TempDir {
    let parent = repo().join("target/catalog-tests");
    std::fs::create_dir_all(&parent).unwrap();
    tempfile::tempdir_in(parent).unwrap()
}

/// The plugin `name` (manifest, sidecar files, compiled component) copied
/// to `<parent>/<name>`; with `without_sidecar` its `[sidecar]` is left
/// out (the test runs the bridge itself and passes `options.sidecar`).
pub fn plugin(name: &str, parent: &Path, without_sidecar: bool) -> PathBuf {
    plugin_with(name, parent, without_sidecar, |_| {})
}

/// [`plugin`] with its manifest changed by `edit` first (a test grants a
/// folder the plugin reads, say).
pub fn plugin_with(
    name: &str,
    parent: &Path,
    without_sidecar: bool,
    edit: impl FnOnce(&mut toml::Table),
) -> PathBuf {
    let source = repo().join("plugins/first-party").join(name);
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let mut manifest: toml::Table = std::fs::read_to_string(source.join("plugin.toml"))
        .unwrap()
        .parse()
        .unwrap();
    if without_sidecar {
        manifest.remove("sidecar");
    } else if source.join("sidecar").is_dir() {
        pmt_core::private_fs::copy_tree(&source.join("sidecar"), &dir.join("sidecar")).unwrap();
    }
    edit(&mut manifest);
    std::fs::write(dir.join("plugin.toml"), manifest.to_string()).unwrap();
    pmt_plugins::testkit::install(&format!("first-party-{name}"), &dir);
    dir
}

/// The plugin `name` installed in a fresh home with `options` and `hosts`
/// granted; the home lives as long as the returned guard.
pub fn install(name: &str, options: &Value, hosts: &[String]) -> (PluginKit, tempfile::TempDir) {
    install_with(name, options, hosts, |_| {})
}

/// [`install`] with the manifest changed by `edit` first.
pub fn install_with(
    name: &str,
    options: &Value,
    hosts: &[String],
    edit: impl FnOnce(&mut toml::Table),
) -> (PluginKit, tempfile::TempDir) {
    let root = scratch();
    let dir = plugin_with(name, root.path(), true, edit);
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let kit = PluginKit::install(&home, &dir, options, hosts).unwrap();
    (kit, root)
}

/// Stores secret `name` in the kit's profile.
pub fn secret(kit: &PluginKit, name: &str, value: &str) {
    kit.setup()
        .scope
        .secrets()
        .store()
        .set(name, &pmt_core::secret::SecretString::new(value))
        .unwrap();
}

/// Runs the conformance suite on plugin `name` against `platform`, with
/// `secrets` stored first.
pub async fn conformance(
    name: &str,
    options: &Value,
    hosts: &[String],
    secrets: &[(&str, &str)],
    platform: Arc<dyn MockPlatform>,
) -> Report {
    let (kit, _root) = install(name, options, hosts);
    for (k, v) in secrets {
        secret(&kit, k, v);
    }
    let adapter = kit.adapter().unwrap();
    kit.run(adapter, platform).await
}

/// Asserts the suite passed, printing it otherwise.
pub fn assert_passed(report: &Report) {
    assert!(report.passed(), "{}", report.render());
}

/// One request the mock received.
#[derive(Debug, Clone)]
pub struct Request {
    /// `GET`, `POST`...
    pub method: String,
    /// Path and query.
    pub path: String,
    /// Header pairs (names lower case).
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl Request {
    /// A header.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The body as JSON.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    /// The body as a form.
    pub fn form(&self) -> Vec<(String, String)> {
        url::form_urlencoded::parse(&self.body)
            .into_owned()
            .collect()
    }

    /// One form field.
    pub fn field(&self, name: &str) -> Option<String> {
        self.form()
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    /// The path without its query.
    pub fn route(&self) -> &str {
        self.path.split('?').next().unwrap_or_default()
    }
}

/// An answer of the mock.
#[derive(Debug, Clone)]
pub struct Reply {
    /// Status code.
    pub status: u16,
    /// Extra headers.
    pub headers: Vec<(String, String)>,
    /// Body.
    pub body: Vec<u8>,
}

impl Reply {
    /// JSON with `status`.
    pub fn json(status: u16, body: &Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_string().into_bytes(),
        }
    }

    /// An empty answer.
    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }
}

/// What a mock answers.
pub type Handler = Arc<dyn Fn(&Request) -> Reply + Send + Sync>;

/// A loopback HTTP server answering with a handler (one request per
/// connection).
pub struct MockHttp {
    base: String,
}

impl MockHttp {
    /// Starts it.
    pub fn start(handler: Handler) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let handler = Arc::clone(&handler);
                std::thread::spawn(move || answer_one(stream, &handler));
            }
        });
        Self { base }
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// `127.0.0.1:<port>`, as a network grant.
    pub fn host(&self) -> String {
        self.base.trim_start_matches("http://").to_owned()
    }
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((k, v)) = header.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_owned()));
        }
    }
    let length = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Request {
        method,
        path,
        headers,
        body,
    })
}

fn answer_one(mut stream: TcpStream, handler: &Handler) {
    let Some(request) = read_request(&stream) else {
        return;
    };
    let reply = handler(&request);
    let mut head = format!(
        "HTTP/1.1 {} X\r\ncontent-length: {}\r\nconnection: close\r\n",
        reply.status,
        reply.body.len()
    );
    for (k, v) in &reply.headers {
        let _ = write!(head, "{k}: {v}\r\n");
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&reply.body);
}

/// A blocking HTTP POST (or other `method`) to `url`: status and body.
pub fn request(method: &str, url: &str, headers: &[(&str, &str)], body: &[u8]) -> (u16, Vec<u8>) {
    let url = url::Url::parse(url).unwrap();
    let address = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let target = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_owned(),
    };
    let mut head = format!(
        "{method} {target} HTTP/1.1\r\nhost: {}\r\ncontent-length: {}\r\nconnection: close\r\n",
        url.host_str().unwrap(),
        body.len()
    );
    for (k, v) in headers {
        let _ = write!(head, "{k}: {v}\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut answer = Vec::new();
    let _ = stream.read_to_end(&mut answer);
    let text = String::from_utf8_lossy(&answer);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.as_bytes().to_vec())
        .unwrap_or_default();
    (status, body)
}

/// A blocking POST.
pub fn post(url: &str, headers: &[(&str, &str)], body: &[u8]) -> (u16, Vec<u8>) {
    request("POST", url, headers, body)
}

/// The socket bridge sidecar (`plugins/first-party/adapter-kit/bridge`)
/// run with the system Node, as the host would run it: a loopback port, a
/// token and the network grant in its environment. Stopped on drop.
pub struct Bridge {
    child: Child,
    /// `options.sidecar` for the adapter.
    pub sidecar: Value,
    /// Its base URL.
    pub base: String,
    /// Its token.
    pub token: String,
}

impl Bridge {
    /// Starts it, allowing `network`.
    pub fn start(network: &[String]) -> Self {
        Self::start_script("plugins/first-party/adapter-kit/bridge/bridge.mjs", network)
    }

    /// Starts the sidecar script at `script` (relative to the repository)
    /// the same way.
    pub fn start_script(script: &str, network: &[String]) -> Self {
        Self::start_script_with_env(script, network, &[])
    }

    /// [`Self::start_script`] with `env` added (the secrets the host
    /// hands a sidecar that declares them).
    pub fn start_script_with_env(script: &str, network: &[String], env: &[(&str, &str)]) -> Self {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let token = "bridge-test-token".to_owned();
        let script = repo().join(script);
        let mut command = Command::new("node");
        // Its own process group: dropping it stops what it started too.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let child = command
            .arg(script)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("PMT_SIDECAR_PORT", port.to_string())
            .env("PMT_SIDECAR_TOKEN", &token)
            .env("PMT_SIDECAR_NETWORK", network.join(","))
            .envs(env.iter().copied())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the socket bridge tests need Node.js 22+ on PATH");
        let base = format!("http://127.0.0.1:{port}");
        for _ in 0..200 {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Self {
            child,
            sidecar: json!({"url": base, "token": token}),
            base,
            token,
        }
    }

    /// Calls the bridge: status and JSON answer.
    pub fn call(&self, path: &str, body: &Value) -> (u16, Value) {
        let auth = format!("Bearer {}", self.token);
        let (status, answer) = post(
            &format!("{}{path}", self.base),
            &[
                ("authorization", &auth),
                ("content-type", "application/json"),
            ],
            body.to_string().as_bytes(),
        );
        (
            status,
            serde_json::from_slice(&answer).unwrap_or(Value::Null),
        )
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        pmt_tools::browser::cleanup::kill_group(self.child.id());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The gateway as an adapter test plays it: inbound events recorded,
/// states kept.
#[derive(Default)]
pub struct Link {
    received: std::sync::Mutex<Vec<pmt_gateway::messaging::Inbound>>,
    states: std::sync::Mutex<Vec<(pmt_gateway::messaging::AdapterState, Option<String>)>>,
}

impl Link {
    /// A new link.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// As the adapter's link.
    pub fn dyn_link(self: &Arc<Self>) -> Arc<dyn pmt_gateway::messaging::AdapterLink> {
        Arc::clone(self) as Arc<dyn pmt_gateway::messaging::AdapterLink>
    }

    /// What was admitted so far.
    pub fn received(&self) -> Vec<pmt_gateway::messaging::Inbound> {
        self.received.lock().unwrap().clone()
    }

    /// The states reported, with their error codes.
    pub fn states(&self) -> Vec<(pmt_gateway::messaging::AdapterState, Option<String>)> {
        self.states.lock().unwrap().clone()
    }

    /// Waits up to 15 s for `count` inbound events, then a little longer
    /// for strays; what arrived.
    pub async fn wait_for(&self, count: usize) -> Vec<pmt_gateway::messaging::Inbound> {
        for _ in 0..300 {
            if self.received.lock().unwrap().len() >= count {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        self.received()
    }
}

impl pmt_gateway::messaging::AdapterLink for Link {
    fn submit(
        &self,
        inbound: pmt_gateway::messaging::Inbound,
    ) -> pmt_gateway::messaging::AdapterFuture<'_, pmt_gateway::messaging::Admission> {
        self.received.lock().unwrap().push(inbound);
        Box::pin(async { pmt_gateway::messaging::Admission::Accepted })
    }

    fn report(
        &self,
        state: pmt_gateway::messaging::AdapterState,
        error: Option<pmt_gateway::messaging::AdapterError>,
    ) {
        self.states
            .lock()
            .unwrap()
            .push((state, error.map(|e| e.code)));
    }
}

/// Serves `adapter`'s HTTP routes as the shared listener does; the prefix.
pub async fn serve(adapter: Arc<dyn pmt_gateway::messaging::Adapter>) -> String {
    pmt_gateway::messaging::conformance::listen(adapter)
        .await
        .unwrap()
}
