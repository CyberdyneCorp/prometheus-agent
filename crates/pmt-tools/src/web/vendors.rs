//! The hosted web backends beside Firecrawl and SearXNG (spec:
//! web-and-media-tools, Built-in web backends with capability flags).
//!
//! Each vendor is a [`VendorSpec`]: where its API lives, how it
//! authenticates and how a search or an extraction is asked and read. A
//! [`Vendor`] is one spec bound to a client, a base URL and a credential:
//! its key from the secrets store (read at each call), none for a keyless
//! vendor or a free tier, or the subscription tool gateway's token.

use std::sync::{Arc, LazyLock};

use pmt_core::secret::SecretString;
use regex::Regex;
use reqwest::header::CONTENT_TYPE;
use serde_json::{Value, json};

use super::backend::{Hit, Page, SecretLookup, WebBackend, WebFuture, hits, json_reply, string};

/// How a vendor's requests carry the credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// `Authorization: Bearer <key>`.
    Bearer,
    /// The key in a named header.
    Header(&'static str),
}

/// How a search is asked and read.
#[derive(Clone, Copy)]
pub struct SearchCall {
    /// Request path under the base URL.
    pub path: &'static str,
    /// GET with `q`/`count` query parameters instead of a JSON POST.
    pub get: bool,
    /// The JSON body of a POST.
    pub body: fn(&str, usize) -> Value,
    /// The hits of a reply.
    pub read: fn(&Value) -> Vec<Hit>,
}

/// How an extraction is asked and read.
#[derive(Clone, Copy)]
pub struct ExtractCall {
    /// Request path under the base URL.
    pub path: &'static str,
    /// The JSON body.
    pub body: fn(&str) -> Value,
    /// The page of a reply.
    pub read: fn(&Value) -> Option<Page>,
}

/// One hosted web API.
#[derive(Clone, Copy)]
pub struct VendorSpec {
    /// The backend id.
    pub id: &'static str,
    /// The API's base URL.
    pub base_url: &'static str,
    /// The secret holding the key (`None`: keyless).
    pub secret: Option<&'static str>,
    /// How the key is sent.
    pub auth: Auth,
    /// Search, when it searches.
    pub search: Option<SearchCall>,
    /// Extraction, when it extracts.
    pub extract: Option<ExtractCall>,
}

fn first_text(item: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| match item.get(*key) {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Array(list)) => list.first().and_then(Value::as_str).map(str::to_owned),
            _ => None,
        })
        .unwrap_or_default()
}

fn list_hits(list: Option<&Value>, snippet_keys: &[&str]) -> Vec<Hit> {
    list.and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("url").is_some())
                .map(|item| Hit {
                    title: string(item.get("title")),
                    url: string(item.get("url")),
                    snippet: first_text(item, snippet_keys),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn first_page(list: Option<&Value>, text_keys: &[&str]) -> Option<Page> {
    let item = list.and_then(Value::as_array)?.first()?;
    Some(Page {
        title: string(item.get("title")),
        markdown: first_text(item, text_keys),
    })
}

/// Tavily: search + extract.
pub const TAVILY: VendorSpec = VendorSpec {
    id: "tavily",
    base_url: "https://api.tavily.com",
    secret: Some("TAVILY_API_KEY"),
    auth: Auth::Bearer,
    search: Some(SearchCall {
        path: "/search",
        get: false,
        body: |q, n| json!({"query": q, "max_results": n}),
        read: |r| list_hits(r.get("results"), &["content"]),
    }),
    extract: Some(ExtractCall {
        path: "/extract",
        body: |u| json!({"urls": [u], "format": "markdown"}),
        read: |r| first_page(r.get("results"), &["raw_content"]),
    }),
};

/// Exa: search + extract.
pub const EXA: VendorSpec = VendorSpec {
    id: "exa",
    base_url: "https://api.exa.ai",
    secret: Some("EXA_API_KEY"),
    auth: Auth::Header("x-api-key"),
    search: Some(SearchCall {
        path: "/search",
        get: false,
        body: |q, n| json!({"query": q, "numResults": n, "contents": {"highlights": true}}),
        read: |r| list_hits(r.get("results"), &["highlights", "text", "summary"]),
    }),
    extract: Some(ExtractCall {
        path: "/contents",
        body: |u| json!({"urls": [u], "text": true}),
        read: |r| first_page(r.get("results"), &["text"]),
    }),
};

/// Parallel: search + extract.
pub const PARALLEL: VendorSpec = VendorSpec {
    id: "parallel",
    base_url: "https://api.parallel.ai",
    secret: Some("PARALLEL_API_KEY"),
    auth: Auth::Header("x-api-key"),
    search: Some(SearchCall {
        path: "/v1beta/search",
        get: false,
        body: |q, n| json!({"objective": q, "search_queries": [q], "max_results": n}),
        read: |r| list_hits(r.get("results"), &["excerpts"]),
    }),
    extract: Some(ExtractCall {
        path: "/v1beta/extract",
        body: |u| json!({"urls": [u], "full_content": true}),
        read: |r| first_page(r.get("results"), &["full_content", "excerpts"]),
    }),
};

/// Keenable: search + extract.
pub const KEENABLE: VendorSpec = VendorSpec {
    id: "keenable",
    base_url: "https://api.keenable.ai",
    secret: Some("KEENABLE_API_KEY"),
    auth: Auth::Bearer,
    search: Some(SearchCall {
        path: "/v1/search",
        get: false,
        body: |q, n| json!({"query": q, "limit": n}),
        read: |r| list_hits(r.get("results"), &["snippet", "description"]),
    }),
    extract: Some(ExtractCall {
        path: "/v1/extract",
        body: |u| json!({"url": u, "format": "markdown"}),
        read: |r| {
            Some(Page {
                title: string(r.get("title")),
                markdown: first_text(r, &["markdown", "content"]),
            })
        },
    }),
};

/// Perplexity: search, and an extraction answered by the query-relevant
/// snippets of the page.
pub const PERPLEXITY: VendorSpec = VendorSpec {
    id: "perplexity",
    base_url: "https://api.perplexity.ai",
    secret: Some("PERPLEXITY_API_KEY"),
    auth: Auth::Bearer,
    search: Some(SearchCall {
        path: "/search",
        get: false,
        body: |q, n| json!({"query": q, "max_results": n}),
        read: |r| list_hits(r.get("results"), &["snippet"]),
    }),
    extract: Some(ExtractCall {
        path: "/search",
        body: |u| json!({"query": u, "max_results": 1, "max_tokens_per_page": 4096}),
        read: |r| first_page(r.get("results"), &["snippet"]),
    }),
};

/// Brave Search's free plan: search only.
pub const BRAVE_FREE: VendorSpec = VendorSpec {
    id: "brave-free",
    base_url: "https://api.search.brave.com",
    secret: Some("BRAVE_SEARCH_API_KEY"),
    auth: Auth::Header("X-Subscription-Token"),
    search: Some(SearchCall {
        path: "/res/v1/web/search",
        get: true,
        body: |_, _| Value::Null,
        read: |r| list_hits(r.pointer("/web/results"), &["description"]),
    }),
    extract: None,
};

/// The subscription tool gateway's web endpoints: search + extract.
pub const NOUS: VendorSpec = VendorSpec {
    id: "nous",
    base_url: "",
    secret: None,
    auth: Auth::Bearer,
    search: Some(SearchCall {
        path: "/v1/web/search",
        get: false,
        body: |q, n| json!({"query": q, "limit": n}),
        read: |r| hits(r.get("results"), "snippet"),
    }),
    extract: Some(ExtractCall {
        path: "/v1/web/extract",
        body: |u| json!({"url": u, "format": "markdown"}),
        read: |r| {
            Some(Page {
                title: string(r.get("title")),
                markdown: string(r.get("markdown")),
            })
        },
    }),
};

/// Firecrawl's hosted API as a keyless-ring vendor (its own backend is
/// [`super::Firecrawl`]).
pub const FIRECRAWL: VendorSpec = VendorSpec {
    id: "firecrawl",
    base_url: super::backend::FIRECRAWL_URL,
    secret: Some(super::backend::FIRECRAWL_SECRET),
    auth: Auth::Bearer,
    search: Some(SearchCall {
        path: "/v1/search",
        get: false,
        body: |q, n| json!({"query": q, "limit": n}),
        read: |r| hits(r.get("data"), "description"),
    }),
    extract: Some(ExtractCall {
        path: "/v1/scrape",
        body: |u| json!({"url": u, "formats": ["markdown"]}),
        read: |r| {
            let data = r.get("data")?;
            Some(Page {
                title: string(data.pointer("/metadata/title")),
                markdown: string(data.get("markdown")),
            })
        },
    }),
};

/// The keyed vendors of this module, by id.
pub const KEYED: &[VendorSpec] = &[TAVILY, EXA, PARALLEL, KEENABLE, PERPLEXITY, BRAVE_FREE];

/// The vendor spec of `id` (keyed vendors, `nous` and `firecrawl`).
#[must_use]
pub fn spec(id: &str) -> Option<&'static VendorSpec> {
    KEYED
        .iter()
        .chain([&NOUS, &FIRECRAWL])
        .find(|spec| spec.id == id)
}

/// Where a vendor's credential comes from.
#[derive(Clone)]
pub enum Credential {
    /// Its key in the secrets store, read at each call.
    Key(SecretLookup),
    /// No credential (a keyless vendor or a free tier).
    Anonymous,
    /// A fixed token (the tool gateway's) and its billing source.
    Token(SecretString, String),
}

/// One vendor bound to a client, base URL and credential.
pub struct Vendor {
    spec: &'static VendorSpec,
    client: reqwest::Client,
    base_url: String,
    credential: Credential,
}

impl Vendor {
    /// `spec` at `base_url` (`None`: the vendor's own) with `credential`.
    #[must_use]
    pub fn new(
        spec: &'static VendorSpec,
        client: reqwest::Client,
        base_url: Option<&str>,
        credential: Credential,
    ) -> Self {
        Self {
            spec,
            client,
            base_url: base_url
                .unwrap_or(spec.base_url)
                .trim_end_matches('/')
                .to_owned(),
            credential,
        }
    }

    /// The vendor's spec.
    #[must_use]
    pub fn spec(&self) -> &'static VendorSpec {
        self.spec
    }

    fn authorize(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, String> {
        let key = match &self.credential {
            Credential::Anonymous => return Ok(request),
            Credential::Token(token, _) => token.clone(),
            Credential::Key(lookup) => {
                let name = self.spec.secret.unwrap_or_default();
                lookup(name).ok_or_else(|| format!("{name} is not set"))?
            }
        };
        Ok(match self.spec.auth {
            Auth::Bearer => request.bearer_auth(key.expose()),
            Auth::Header(name) => request.header(name, key.expose()),
        })
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        let request = self
            .client
            .post(format!("{}{path}", self.base_url))
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string());
        json_reply(self.authorize(request)?.send().await).await
    }

    async fn get(&self, path: &str, query: &str, limit: usize) -> Result<Value, String> {
        let url = url::Url::parse_with_params(
            &format!("{}{path}", self.base_url),
            [("q", query), ("count", &limit.to_string())],
        )
        .map_err(|err| format!("invalid {} URL: {err}", self.spec.id))?;
        let request = self.client.get(url);
        json_reply(self.authorize(request)?.send().await).await
    }
}

impl WebBackend for Vendor {
    fn id(&self) -> &str {
        self.spec.id
    }

    fn can_extract(&self) -> bool {
        self.spec.extract.is_some()
    }

    fn billing_source(&self) -> String {
        match &self.credential {
            Credential::Token(_, billing) => billing.clone(),
            Credential::Anonymous => format!("keyless:{}", self.spec.id),
            Credential::Key(_) => format!("api_key:{}", self.spec.id),
        }
    }

    fn search<'a>(&'a self, query: &'a str, limit: usize) -> WebFuture<'a, Vec<Hit>> {
        Box::pin(async move {
            let call = self
                .spec
                .search
                .ok_or_else(|| format!("{} cannot search", self.spec.id))?;
            let reply = if call.get {
                self.get(call.path, query, limit).await?
            } else {
                self.post(call.path, &(call.body)(query, limit)).await?
            };
            Ok((call.read)(&reply))
        })
    }

    fn extract<'a>(&'a self, url: &'a str) -> WebFuture<'a, Page> {
        Box::pin(async move {
            let call = self
                .spec
                .extract
                .ok_or_else(|| format!("{} cannot extract pages", self.spec.id))?;
            let reply = self.post(call.path, &(call.body)(url)).await?;
            (call.read)(&reply).ok_or_else(|| "the reply holds no page".to_owned())
        })
    }
}

/// `DuckDuckGo`'s HTML results: keyless search.
pub struct Ddgs {
    client: reqwest::Client,
    base_url: String,
}

/// The HTML endpoint `ddgs` reads.
pub const DDGS_URL: &str = "https://html.duckduckgo.com";

static RESULT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?s)class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>.*?class="result__snippet"[^>]*>(.*?)</a>"#,
    )
    .expect("valid result pattern")
});
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").expect("valid tag pattern"));

impl Ddgs {
    /// A client of `base_url` (`None`: `DuckDuckGo`).
    #[must_use]
    pub fn new(client: reqwest::Client, base_url: Option<&str>) -> Self {
        Self {
            client,
            base_url: base_url
                .unwrap_or(DDGS_URL)
                .trim_end_matches('/')
                .to_owned(),
        }
    }
}

/// Text of an HTML fragment: tags dropped, the common entities decoded.
fn html_text(fragment: &str) -> String {
    TAG.replace_all(fragment, "")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .trim()
        .to_owned()
}

/// The target of a `DuckDuckGo` redirect link (`//duckduckgo.com/l/?uddg=`).
fn result_url(href: &str) -> String {
    let absolute = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href.to_owned()
    };
    url::Url::parse(&absolute)
        .ok()
        .and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == "uddg")
                .map(|(_, v)| v.into_owned())
        })
        .unwrap_or(absolute)
}

impl WebBackend for Ddgs {
    fn id(&self) -> &'static str {
        "ddgs"
    }

    fn can_extract(&self) -> bool {
        false
    }

    fn billing_source(&self) -> String {
        "keyless:ddgs".to_owned()
    }

    fn search<'a>(&'a self, query: &'a str, _limit: usize) -> WebFuture<'a, Vec<Hit>> {
        Box::pin(async move {
            let form = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("q", query)
                .finish();
            let response = self
                .client
                .post(format!("{}/html/", self.base_url))
                .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(form)
                .send()
                .await
                .map_err(|e| super::backend::without_url(&e))?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("HTTP {}", status.as_u16()));
            }
            let html = response
                .text()
                .await
                .map_err(|e| super::backend::without_url(&e))?;
            Ok(RESULT
                .captures_iter(&html)
                .map(|c| Hit {
                    url: result_url(&html_text(&c[1])),
                    title: html_text(&c[2]),
                    snippet: html_text(&c[3]),
                })
                .collect())
        })
    }

    fn extract<'a>(&'a self, _url: &'a str) -> WebFuture<'a, Page> {
        Box::pin(async { Err("ddgs cannot extract pages".to_owned()) })
    }
}

/// xAI Responses with the built-in web search: search only.
pub struct XaiSearch {
    client: reqwest::Client,
    base_url: String,
    config: pmt_config::schema::WebXaiConfig,
    credential: Arc<dyn Fn() -> Option<(SecretString, String)> + Send + Sync>,
}

/// The xAI API.
pub const XAI_URL: &str = "https://api.x.ai/v1";

impl XaiSearch {
    /// A client of `base_url` (`None`: xAI) answering with the credential
    /// `credential` returns (its token and billing source).
    #[must_use]
    pub fn new(
        client: reqwest::Client,
        base_url: Option<&str>,
        config: pmt_config::schema::WebXaiConfig,
        credential: Arc<dyn Fn() -> Option<(SecretString, String)> + Send + Sync>,
    ) -> Self {
        Self {
            client,
            base_url: base_url.unwrap_or(XAI_URL).trim_end_matches('/').to_owned(),
            config,
            credential,
        }
    }

    fn tool(&self) -> Value {
        let mut tool = json!({"type": "web_search"});
        if !self.config.allowed_domains.is_empty() {
            tool["filters"] = json!({"allowed_domains": self.config.allowed_domains});
        } else if !self.config.excluded_domains.is_empty() {
            tool["filters"] = json!({"excluded_domains": self.config.excluded_domains});
        }
        tool
    }
}

/// The URL citations of an xAI Responses reply, in order, deduplicated:
/// the `url_citation` annotations of its output, else its `citations`
/// list.
#[must_use]
pub fn citations(reply: &Value) -> Vec<Hit> {
    let mut found: Vec<Hit> = Vec::new();
    for note in annotations(reply) {
        let url = string(note.get("url"));
        if !url.is_empty() && found.iter().all(|h| h.url != url) {
            found.push(Hit {
                title: string(note.get("title")),
                url,
                snippet: String::new(),
            });
        }
    }
    if found.is_empty() {
        let listed = reply.get("citations").and_then(Value::as_array);
        found = listed
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|url| Hit {
                title: String::new(),
                url: url.to_owned(),
                snippet: String::new(),
            })
            .collect();
    }
    found
}

/// Every annotation of every content part of a Responses reply's output.
fn annotations(reply: &Value) -> impl Iterator<Item = &Value> {
    reply
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|output| output.get("content").and_then(Value::as_array))
        .flatten()
        .filter_map(|part| part.get("annotations").and_then(Value::as_array))
        .flatten()
}

impl WebBackend for XaiSearch {
    fn id(&self) -> &'static str {
        "xai"
    }

    fn can_extract(&self) -> bool {
        false
    }

    fn billing_source(&self) -> String {
        (self.credential)().map_or_else(|| "api_key:xai".to_owned(), |(_, billing)| billing)
    }

    fn search<'a>(&'a self, query: &'a str, _limit: usize) -> WebFuture<'a, Vec<Hit>> {
        Box::pin(async move {
            let (token, _) = (self.credential)()
                .ok_or_else(|| "no xAI API key or xai-oauth sign-in".to_owned())?;
            let body = json!({
                "model": self.config.model,
                "input": [{"role": "user", "content": query}],
                "tools": [self.tool()],
            });
            let sent = self
                .client
                .post(format!("{}/responses", self.base_url))
                .bearer_auth(token.expose())
                .header(CONTENT_TYPE, "application/json")
                .timeout(std::time::Duration::from_secs(self.config.timeout_secs))
                .body(body.to_string())
                .send()
                .await;
            Ok(citations(&json_reply(sent).await?))
        })
    }

    fn extract<'a>(&'a self, _url: &'a str) -> WebFuture<'a, Page> {
        Box::pin(async { Err("xai cannot extract pages".to_owned()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddgs_results_are_read_from_html() {
        let html = r#"<div><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fa.test%2Fx&amp;rut=1">A <b>title</b></a>
            <a class="result__snippet" href="x">Some &amp; snippet</a></div>"#;
        let c = RESULT.captures(html).unwrap();
        assert_eq!(result_url(&html_text(&c[1])), "https://a.test/x");
        assert_eq!(html_text(&c[2]), "A title");
        assert_eq!(html_text(&c[3]), "Some & snippet");
    }

    #[test]
    fn xai_citations_come_from_annotations_then_the_citation_list() {
        let reply = json!({"output": [{"content": [{"annotations": [
            {"type": "url_citation", "url": "https://a.test", "title": "A"},
            {"type": "url_citation", "url": "https://a.test", "title": "A"}]}]}]});
        assert_eq!(citations(&reply).len(), 1);
        let reply = json!({"citations": ["https://b.test"]});
        assert_eq!(citations(&reply)[0].url, "https://b.test");
    }
}
