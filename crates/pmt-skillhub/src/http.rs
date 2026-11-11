//! The hub's HTTP access: one small blocking interface, so every source
//! and test fake speaks the same shape, and the hub's endpoints.
//!
//! [`Client`] runs each request on its own thread with a current-thread
//! runtime: hub commands are called from plain CLI code and from chat
//! slash commands that already run inside a runtime, and blocking on a
//! nested runtime there would panic.

use std::time::Duration;

use pmt_core::error::{Error, codes};
use pmt_net::http::OutboundPolicy;

/// One request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// `GET`, `POST`, `PUT` or `PATCH`.
    pub method: &'static str,
    /// The absolute URL.
    pub url: String,
    /// Extra headers.
    pub headers: Vec<(String, String)>,
    /// The body, if any.
    pub body: Option<Vec<u8>>,
    /// The deadline of this request.
    pub timeout: Duration,
}

impl Request {
    /// A `GET` with the default timeout.
    #[must_use]
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: "GET",
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Adds a header.
    #[must_use]
    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_owned(), value.into()));
        self
    }

    /// A JSON body with `method`.
    #[must_use]
    pub fn json(mut self, method: &'static str, body: &serde_json::Value) -> Self {
        self.method = method;
        self.body = Some(body.to_string().into_bytes());
        self.header("content-type", "application/json")
    }

    /// Another deadline.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// An answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The HTTP status.
    pub status: u16,
    /// The body.
    pub body: Vec<u8>,
}

impl Response {
    /// Whether the status is 2xx.
    #[must_use]
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as JSON.
    #[must_use]
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

/// The default deadline of one request.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

/// Most bytes one download may hold (a skill archive or file).
pub const MAX_BYTES: usize = 25 * 1024 * 1024;

/// Blocking HTTP.
pub trait Http: Send + Sync {
    /// Sends `request`.
    ///
    /// # Errors
    ///
    /// A transport failure (no answer at all).
    fn send(&self, request: Request) -> Result<Response, String>;
}

/// The real client, built from the profile's outbound policy.
#[derive(Debug, Clone)]
pub struct Client {
    client: reqwest::Client,
}

impl Client {
    /// A client following `policy` (`network.*`).
    ///
    /// # Errors
    ///
    /// `skill.hub_unavailable` when the client cannot be built.
    pub fn new(policy: &OutboundPolicy) -> Result<Self, Error> {
        let client = pmt_net::http::client_builder(policy)
            .and_then(|b| {
                b.user_agent(concat!("prometheus/", env!("CARGO_PKG_VERSION")))
                    .build()
                    .map_err(|e| Error::new(&codes::SKILL_HUB_UNAVAILABLE, e.to_string()))
            })
            .map_err(|e| Error::new(&codes::SKILL_HUB_UNAVAILABLE, e.message().to_owned()))?;
        Ok(Self { client })
    }
}

impl Http for Client {
    fn send(&self, request: Request) -> Result<Response, String> {
        let client = self.client.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            runtime.block_on(send(&client, request))
        })
        .join()
        .map_err(|_| "the request thread panicked".to_owned())?
    }
}

async fn send(client: &reqwest::Client, request: Request) -> Result<Response, String> {
    let method =
        reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|e| e.to_string())?;
    let mut builder = client
        .request(method, &request.url)
        .timeout(request.timeout);
    for (name, value) in &request.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    if let Some(body) = request.body {
        builder = builder.body(body);
    }
    let response = builder.send().await.map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    if response
        .content_length()
        .is_some_and(|n| n > MAX_BYTES as u64)
    {
        return Err(format!("{}: larger than {MAX_BYTES} bytes", request.url));
    }
    let body = response.bytes().await.map_err(|e| e.to_string())?;
    if body.len() > MAX_BYTES {
        return Err(format!("{}: larger than {MAX_BYTES} bytes", request.url));
    }
    Ok(Response {
        status,
        body: body.to_vec(),
    })
}

/// Where each hub service lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// The GitHub REST API.
    pub github_api: String,
    /// Raw GitHub file contents (`<raw>/<owner>/<repo>/<ref>/<path>`).
    pub github_raw: String,
    /// The skills.sh directory.
    pub skills_sh: String,
    /// The ClawHub API (`.../api/v1`).
    pub clawhub: String,
    /// LobeHub's agent index host.
    pub lobehub: String,
    /// browse.sh.
    pub browse_sh: String,
    /// The central skills index (JSON).
    pub index: String,
}

/// The central skills index of the project's repository.
pub const DEFAULT_INDEX_URL: &str = "https://raw.githubusercontent.com/CyberdyneCorp/prometheus-agent/main/crates/pmt-skillhub/index/skills-index.json";

impl Endpoints {
    /// The public services.
    #[must_use]
    pub fn public() -> Self {
        Self {
            github_api: "https://api.github.com".to_owned(),
            github_raw: "https://raw.githubusercontent.com".to_owned(),
            skills_sh: "https://skills.sh".to_owned(),
            clawhub: "https://clawhub.ai/api/v1".to_owned(),
            lobehub: "https://chat-agents.lobehub.com".to_owned(),
            browse_sh: "https://browse.sh".to_owned(),
            index: DEFAULT_INDEX_URL.to_owned(),
        }
    }

    /// Every service under one base URL (a local fake: `<base>/github`,
    /// `<base>/raw`, ...).
    #[must_use]
    pub fn under(base: &str) -> Self {
        let base = base.trim_end_matches('/');
        Self {
            github_api: format!("{base}/github"),
            github_raw: format!("{base}/raw"),
            skills_sh: format!("{base}/skills-sh"),
            clawhub: format!("{base}/clawhub/api/v1"),
            lobehub: format!("{base}/lobehub"),
            browse_sh: format!("{base}/browse-sh"),
            index: format!("{base}/index.json"),
        }
    }
}

/// `GET url`, as text; `None` for a 404.
///
/// # Errors
///
/// `skill.hub_unavailable` on a transport failure or another status.
pub fn get_bytes(http: &dyn Http, request: Request) -> Result<Option<Vec<u8>>, Error> {
    let url = request.url.clone();
    let response = http.send(request).map_err(|e| unavailable(&url, &e))?;
    if response.status == 404 {
        return Ok(None);
    }
    if !response.ok() {
        return Err(unavailable(&url, &format!("answered {}", response.status)));
    }
    Ok(Some(response.body))
}

/// `GET url` as JSON; `None` for a 404.
///
/// # Errors
///
/// As [`get_bytes`], and for a body that is not JSON.
pub fn get_json(http: &dyn Http, request: Request) -> Result<Option<serde_json::Value>, Error> {
    let url = request.url.clone();
    get_bytes(http, request)?
        .map(|body| serde_json::from_slice(&body).map_err(|e| unavailable(&url, &e.to_string())))
        .transpose()
}

/// The error of an unreachable or failing hub request.
#[must_use]
pub fn unavailable(url: &str, why: &str) -> Error {
    Error::new(&codes::SKILL_HUB_UNAVAILABLE, format!("{url}: {why}"))
}
