//! Web backends: who answers `web_search` and `web_extract`.
//!
//! Each backend is a typed implementation of [`WebBackend`] selected by
//! id from `[web]` config. Their API endpoints are operator configuration;
//! the URLs the model asks to extract are checked by the session's
//! [`pmt_net::fetch::Fetcher`] before a backend sees them.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use pmt_core::secret::SecretString;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::{Value, json};

/// The hosted Firecrawl API.
pub const FIRECRAWL_URL: &str = "https://api.firecrawl.dev";

/// The secret holding the hosted Firecrawl API key.
pub const FIRECRAWL_SECRET: &str = "FIRECRAWL_API_KEY";

/// What a backend call returns.
pub type WebFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

/// Reads a secret at each use (never cached).
pub type SecretLookup = Arc<dyn Fn(&str) -> Option<SecretString> + Send + Sync>;

/// One search hit, in backend rank order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Page title.
    pub title: String,
    /// Page URL.
    pub url: String,
    /// The backend's snippet.
    pub snippet: String,
}

/// One extracted page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// Page title.
    pub title: String,
    /// Page content as Markdown.
    pub markdown: String,
}

/// A web search and/or extraction service.
pub trait WebBackend: Send + Sync {
    /// The backend id (`firecrawl`).
    fn id(&self) -> &str;
    /// Whether it can extract pages (search-only backends cannot).
    fn can_extract(&self) -> bool;
    /// Searches; hits in rank order, possibly more than `limit`.
    fn search<'a>(&'a self, query: &'a str, limit: usize) -> WebFuture<'a, Vec<Hit>>;
    /// Extracts one page.
    fn extract<'a>(&'a self, url: &'a str) -> WebFuture<'a, Page>;
}

/// Firecrawl (hosted or self-hosted): search and extract.
pub struct Firecrawl {
    client: reqwest::Client,
    base_url: String,
    secrets: SecretLookup,
    self_hosted: bool,
}

impl Firecrawl {
    /// A client of `base_url` (`None`: the hosted API, which needs
    /// [`FIRECRAWL_SECRET`]).
    #[must_use]
    pub fn new(client: reqwest::Client, base_url: Option<&str>, secrets: SecretLookup) -> Self {
        Self {
            client,
            base_url: base_url
                .unwrap_or(FIRECRAWL_URL)
                .trim_end_matches('/')
                .to_owned(),
            secrets,
            self_hosted: base_url.is_some(),
        }
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        let mut request = self
            .client
            .post(format!("{}{path}", self.base_url))
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string());
        match (self.secrets)(FIRECRAWL_SECRET) {
            Some(key) => {
                request = request.header(AUTHORIZATION, format!("Bearer {}", key.expose()));
            }
            None if !self.self_hosted => return Err(format!("{FIRECRAWL_SECRET} is not set")),
            None => {}
        }
        let reply = json_reply(request.send().await).await?;
        if reply.get("success").and_then(Value::as_bool) == Some(false) {
            let why = reply
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("request failed");
            return Err(why.to_owned());
        }
        Ok(reply)
    }
}

impl WebBackend for Firecrawl {
    fn id(&self) -> &'static str {
        "firecrawl"
    }

    fn can_extract(&self) -> bool {
        true
    }

    fn search<'a>(&'a self, query: &'a str, limit: usize) -> WebFuture<'a, Vec<Hit>> {
        Box::pin(async move {
            let reply = self
                .post("/v1/search", json!({"query": query, "limit": limit}))
                .await?;
            Ok(hits(reply.get("data"), "description"))
        })
    }

    fn extract<'a>(&'a self, url: &'a str) -> WebFuture<'a, Page> {
        Box::pin(async move {
            let reply = self
                .post("/v1/scrape", json!({"url": url, "formats": ["markdown"]}))
                .await?;
            let data = reply.get("data").cloned().unwrap_or_default();
            Ok(Page {
                title: string(data.pointer("/metadata/title")),
                markdown: string(data.get("markdown")),
            })
        })
    }
}

/// A SearXNG instance: search only.
pub struct Searxng {
    client: reqwest::Client,
    url: String,
}

impl Searxng {
    /// A client of the instance at `url`.
    #[must_use]
    pub fn new(client: reqwest::Client, url: &str) -> Self {
        Self {
            client,
            url: url.trim_end_matches('/').to_owned(),
        }
    }
}

impl WebBackend for Searxng {
    fn id(&self) -> &'static str {
        "searxng"
    }

    fn can_extract(&self) -> bool {
        false
    }

    fn search<'a>(&'a self, query: &'a str, _limit: usize) -> WebFuture<'a, Vec<Hit>> {
        Box::pin(async move {
            let url = url::Url::parse_with_params(
                &format!("{}/search", self.url),
                [("q", query), ("format", "json")],
            )
            .map_err(|err| format!("invalid web.searxng.url: {err}"))?;
            let reply = json_reply(self.client.get(url).send().await).await?;
            Ok(hits(reply.get("results"), "content"))
        })
    }

    fn extract<'a>(&'a self, _url: &'a str) -> WebFuture<'a, Page> {
        Box::pin(async { Err("searxng cannot extract pages".to_owned()) })
    }
}

async fn json_reply(sent: reqwest::Result<reqwest::Response>) -> Result<Value, String> {
    let response = sent.map_err(|err| without_url(&err))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    let body = response.text().await.map_err(|err| without_url(&err))?;
    serde_json::from_str(&body).map_err(|err| format!("unreadable reply: {err}"))
}

/// A transport error without its URL (which may carry a query string).
fn without_url(err: &reqwest::Error) -> String {
    let kind = if err.is_timeout() {
        "timed out"
    } else if err.is_connect() {
        "connection failed"
    } else {
        "request failed"
    };
    kind.to_owned()
}

fn hits(list: Option<&Value>, snippet_key: &str) -> Vec<Hit> {
    list.and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("url").is_some())
                .map(|item| Hit {
                    title: string(item.get("title")),
                    url: string(item.get("url")),
                    snippet: string(item.get(snippet_key)),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn string(value: Option<&Value>) -> String {
    value.and_then(Value::as_str).unwrap_or_default().to_owned()
}
