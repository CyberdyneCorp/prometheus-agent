//! Connecting to the user's own browser (spec: execution-environments,
//! Connecting to the user's browser; Browser session resolution).
//!
//! `/browser connect [<url>]` binds a chat session to an explicit DevTools
//! endpoint: a `ws://` URL as given, or an `http://` one resolved through
//! its `/json/version`. Without a URL it uses `http://127.0.0.1:9222`,
//! and when nothing listens there it starts a Chromium-family browser of
//! the host with that debugging port and a dedicated profile under
//! `<profile>/cache/browser/connect/` (Chrome 136+ refuses remote
//! debugging on the default profile). That browser belongs to the user:
//! disconnecting detaches and never closes it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use super::cdp::Cdp;

/// The endpoint `/browser connect` uses without a URL.
pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:9222";
/// The debugging port of a browser `/browser connect` starts.
pub const DEFAULT_PORT: u16 = 9222;
/// How long a started browser may take to open its debugging port.
const START_WAIT: Duration = Duration::from_secs(30);

/// Chat sessions of this process bound to an explicit endpoint (the
/// `browser_cdp` probe's process-wide answer).
static CONNECTED: AtomicUsize = AtomicUsize::new(0);

/// Whether a session of this process is connected with `/browser connect`.
#[must_use]
pub fn any_connected() -> bool {
    CONNECTED.load(Ordering::SeqCst) > 0
}

/// Counts one connected session while alive.
#[derive(Debug)]
pub(super) struct Counted;

impl Counted {
    pub(super) fn new() -> Self {
        CONNECTED.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        CONNECTED.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The browser-level WebSocket endpoint of `endpoint` (`ws://`/`wss://`
/// as given, `http://`/`https://` through `/json/version`).
///
/// # Errors
///
/// Unusable URL, nothing answering, or no WebSocket URL in the answer.
pub async fn websocket_url(endpoint: &str) -> Result<String, String> {
    let parsed = url::Url::parse(endpoint).map_err(|_| format!("not a URL: {endpoint}"))?;
    match parsed.scheme() {
        "ws" | "wss" => Ok(endpoint.to_owned()),
        "http" | "https" => {
            let version = format!("{}/json/version", endpoint.trim_end_matches('/'));
            let client = pmt_net::client_builder(&pmt_net::OutboundPolicy::default())
                .map_err(|err| format!("no HTTP client: {}", err.message()))?
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .map_err(|err| format!("no HTTP client: {err}"))?;
            let body = client
                .get(&version)
                .send()
                .await
                .map_err(|_| format!("nothing answers at {endpoint}"))?
                .text()
                .await
                .map_err(|_| format!("nothing answers at {endpoint}"))?;
            let reply: Value = serde_json::from_str(&body)
                .map_err(|_| format!("{endpoint} is not a DevTools endpoint"))?;
            reply
                .get("webSocketDebuggerUrl")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("{endpoint} gave no webSocketDebuggerUrl"))
        }
        other => Err(format!(
            "unsupported endpoint scheme {other}: use ws://, wss://, http:// or https://"
        )),
    }
}

/// Where `/browser connect` without a URL starts a browser.
#[derive(Debug, Clone)]
pub struct Starter {
    /// Chromium-family executables of the host, in preference order.
    pub candidates: Vec<PathBuf>,
    /// `<profile>/cache/browser`.
    pub cache_dir: PathBuf,
    /// The debugging port ([`DEFAULT_PORT`]).
    pub port: u16,
    /// No display: the started browser runs headless (it could open no
    /// window anyway).
    pub headless: bool,
}

impl Starter {
    /// The starter of `/browser connect`: port 9222, headless without a
    /// display.
    #[must_use]
    pub fn new(candidates: Vec<PathBuf>, cache_dir: PathBuf, display: bool) -> Self {
        Self {
            candidates,
            cache_dir,
            port: DEFAULT_PORT,
            headless: !display,
        }
    }

    /// The HTTP endpoint of the port.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The dedicated profile directory of a started browser.
    #[must_use]
    pub fn profile_dir(&self) -> PathBuf {
        self.cache_dir.join("connect")
    }

    /// Starts the first installed candidate with the default debugging
    /// port on the dedicated profile, detached from this process (it
    /// outlives a disconnect and this process), and waits for its port.
    ///
    /// # Errors
    ///
    /// No browser installed, it could not start, or its port did not open.
    pub async fn start(&self) -> Result<String, String> {
        let executable = self
            .candidates
            .iter()
            .find(|p| p.is_file())
            .ok_or("no Chromium-family browser (Chrome, Brave, Edge, Chromium) is installed")?;
        let profile = self.profile_dir();
        pmt_core::private_fs::create_dir(&profile)
            .map_err(|err| format!("cannot create {}: {err}", profile.display()))?;
        spawn_detached(executable, &profile, self.port, self.headless)?;
        let deadline = tokio::time::Instant::now() + START_WAIT;
        loop {
            if let Ok(url) = websocket_url(&self.endpoint()).await {
                return Ok(url);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "{} did not open port {} within {} s",
                    executable.display(),
                    self.port,
                    START_WAIT.as_secs()
                ));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

fn spawn_detached(
    executable: &Path,
    profile: &Path,
    port: u16,
    headless: bool,
) -> Result<(), String> {
    let mut flag = std::ffi::OsString::from("--user-data-dir=");
    flag.push(profile);
    let mut command = std::process::Command::new(executable);
    if headless {
        command.arg("--headless=new");
    }
    command
        .arg(format!("--remote-debugging-port={port}"))
        .arg(flag)
        .args(["--no-first-run", "--no-default-browser-check"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Its own process group: a Ctrl-C of this process does not reach it.
    super::cleanup::new_group(&mut command);
    let child = command
        .spawn()
        .map_err(|err| format!("cannot start {}: {err}", executable.display()))?;
    // Never waited for: the user's browser is not ours to reap or kill.
    drop(child);
    Ok(())
}

/// Resolves what `/browser connect [<url>]` binds to: the given endpoint,
/// else the starter's (`http://127.0.0.1:9222`), starting a browser when
/// nothing listens there. Returns the endpoint to bind.
///
/// # Errors
///
/// The endpoint cannot be reached, or no browser could be started.
pub async fn resolve(url: Option<&str>, starter: &Starter) -> Result<String, String> {
    if let Some(url) = url {
        websocket_url(url).await?;
        return Ok(url.to_owned());
    }
    let endpoint = starter.endpoint();
    if websocket_url(&endpoint).await.is_err() {
        starter.start().await?;
    }
    Ok(endpoint)
}

/// What `/browser status` shows of a connected browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// The endpoint.
    pub endpoint: String,
    /// `Browser.getVersion`'s product (`Chrome/138.0...`).
    pub version: String,
    /// Open targets: type, title, URL.
    pub targets: Vec<(String, String, String)>,
}

impl Status {
    /// Reads the status of the browser at `endpoint` over a connection of
    /// its own.
    ///
    /// # Errors
    ///
    /// The browser cannot be reached.
    pub async fn read(endpoint: &str) -> Result<Self, String> {
        let cdp = Cdp::connect(&websocket_url(endpoint).await?).await?;
        let version = cdp.call("Browser.getVersion", json!({}), None).await?;
        let targets = cdp.call("Target.getTargets", json!({}), None).await?;
        let text = |v: &Value, k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        Ok(Self {
            endpoint: endpoint.to_owned(),
            version: text(&version, "product"),
            targets: targets["targetInfos"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .map(|t| (text(t, "type"), text(t, "title"), text(t, "url")))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// The status as lines.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = format!(
            "connected to {}\nbrowser: {}\ntargets: {}",
            self.endpoint,
            self.version,
            self.targets.len()
        );
        for (kind, title, url) in &self.targets {
            let _ = write!(text, "\n  {kind}  {title}  {url}");
        }
        text
    }
}

/// Chromium-family browsers `/browser connect` may start: the host's
/// usual installations (on Windows too: the user's browser is not
/// confined).
#[must_use]
pub fn candidates(path_var: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let mut found = super::launch::system_candidates(path_var);
    if cfg!(windows) {
        for root in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            let Some(base) = std::env::var_os(root) else {
                continue;
            };
            for exe in [
                r"Google\Chrome\Application\chrome.exe",
                r"BraveSoftware\Brave-Browser\Application\brave.exe",
                r"Microsoft\Edge\Application\msedge.exe",
                r"Chromium\Application\chrome.exe",
            ] {
                found.push(PathBuf::from(&base).join(exe));
            }
        }
    }
    found
}
