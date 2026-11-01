//! `web_search` and `web_extract` as registry entries.

use std::sync::{Arc, LazyLock};

use pmt_core::capability::Capability as Cap;
use regex::Regex;
use serde_json::{Value, json};

use super::{Capability, WebSession, resolve, window::window};
use crate::descriptor::{
    CallContext, Effect, HandlerFuture, Owner, RegisteredTool, ResourceClaims, ToolDescriptor,
    ToolHandler,
};
use crate::environments::{Environments, part};
use crate::probe::FnProbe;
use crate::result::{Status, ToolResult};

/// The toolset of both tools.
pub const TOOLSET: &str = "web";
/// Most hits `web_search` returns.
pub const MAX_RESULTS: usize = 100;
/// Hits `web_search` returns by default.
pub const DEFAULT_RESULTS: usize = 5;
/// Most URLs per `web_extract` call.
pub const MAX_URLS: usize = 5;
/// Bounds of the per-page character budget.
pub const CHAR_LIMIT_RANGE: (usize, usize) = (2_000, 500_000);

/// Markdown images: `![alt](src)`.
static IMAGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!\[([^\]]*)\]\([^)]*\)").expect("valid image pattern"));

struct SearchHandler(Arc<Environments>);
struct ExtractHandler(Arc<Environments>);

/// `web_search` and `web_extract`, each probed for a configured backend.
#[must_use]
pub fn tools(environments: &Arc<Environments>) -> Vec<RegisteredTool> {
    let probe = |capability: Capability, id: &str| {
        Arc::new(FnProbe::new(
            id,
            move |context: crate::probe::ProbeContext<'_>| {
                resolve(&context.config.web, capability, context.has_secret).map(|_| ())
            },
        ))
    };
    vec![
        RegisteredTool::new(
            search_descriptor(),
            Owner::Builtin,
            Arc::new(SearchHandler(Arc::clone(environments))),
        )
        .with_probe(probe(Capability::Search, "web.search_backend")),
        RegisteredTool::new(
            extract_descriptor(),
            Owner::Builtin,
            Arc::new(ExtractHandler(Arc::clone(environments))),
        )
        .with_probe(probe(Capability::Extract, "web.extract_backend")),
    ]
}

impl ToolHandler for SearchHandler {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let web = part(&self.0, context, "web", |env| env.web.clone())?;
            let query = arguments
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let limit = arguments
                .get("limit")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(DEFAULT_RESULTS)
                .clamp(1, MAX_RESULTS);
            Ok(web.search(query, limit).await)
        })
    }
}

impl ToolHandler for ExtractHandler {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let web = part(&self.0, context, "web", |env| env.web.clone())?;
            let urls: Vec<String> = arguments
                .get("urls")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let char_limit = arguments
                .get("char_limit")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok());
            Ok(web.extract(&urls, char_limit).await)
        })
    }
}

impl WebSession {
    /// Runs one search.
    pub async fn search(&self, query: &str, limit: usize) -> ToolResult {
        let backend = match &self.search {
            Ok(backend) => backend,
            Err(why) => return ToolResult::error(format!("web_search unavailable: {why}")),
        };
        match backend.search(query, limit).await {
            Ok(hits) => {
                let results: Vec<Value> = hits
                    .into_iter()
                    .take(limit)
                    .enumerate()
                    .map(|(index, hit)| {
                        json!({"title": hit.title, "url": hit.url, "snippet": hit.snippet, "position": index + 1})
                    })
                    .collect();
                ToolResult::ok(json!({"results": results, "backend": backend.id()}).to_string())
            }
            Err(why) => ToolResult::error(format!("web_search failed ({}): {why}", backend.id())),
        }
    }

    /// Extracts each URL; one failing URL never fails the others.
    pub async fn extract(&self, urls: &[String], char_limit: Option<usize>) -> ToolResult {
        let backend = match &self.extract {
            Ok(backend) => backend,
            Err(why) => return ToolResult::error(format!("web_extract unavailable: {why}")),
        };
        let budget = char_limit
            .unwrap_or_else(|| {
                usize::try_from(self.config.extract_char_limit).unwrap_or(usize::MAX)
            })
            .clamp(CHAR_LIMIT_RANGE.0, CHAR_LIMIT_RANGE.1);
        let mut results = Vec::with_capacity(urls.len());
        for url in urls {
            results.push(self.extract_one(backend.as_ref(), url, budget).await);
        }
        let any_ok = results.iter().any(|r| r["status"] == "ok");
        let status = if any_ok { Status::Ok } else { Status::Error };
        ToolResult::text(
            status,
            json!({"results": results, "backend": backend.id()}).to_string(),
        )
    }

    async fn extract_one(
        &self,
        backend: &dyn super::WebBackend,
        url: &str,
        budget: usize,
    ) -> Value {
        if let Err(err) = self.fetcher.check(url).await {
            return json!({"url": url, "status": "blocked", "error": err.message()});
        }
        let call = backend.extract(url);
        let outcome = match self.extract_timeout() {
            Some(limit) => tokio::time::timeout(limit, call).await.ok(),
            None => Some(call.await),
        };
        match outcome {
            None => json!({"url": url, "status": "timeout", "error": format!(
                "no answer within {} s (web.extract_timeout_secs)", self.config.extract_timeout_secs)}),
            Some(Err(why)) => json!({"url": url, "status": "error", "error": format!(
                "web_extract failed ({}): {why}", backend.id())}),
            Some(Ok(page)) => self.page_entry(backend.id(), url, &page, budget),
        }
    }

    /// The entry of an extracted page: whole within `budget`, else the
    /// head/tail window over the full text stored on disk.
    fn page_entry(&self, backend: &str, url: &str, page: &super::Page, budget: usize) -> Value {
        let text = IMAGE.replace_all(&page.markdown, "[IMAGE: $1]");
        let content = if text.chars().count() <= budget {
            text.into_owned()
        } else {
            match self.full_text.save(url, FORMAT, backend, &text) {
                Ok(stored) => window(&text, budget, &stored),
                Err(err) => {
                    return json!({"url": url, "status": "error", "error": format!(
                        "web_extract could not store the full text: {err}")});
                }
            }
        };
        json!({"url": url, "title": page.title, "content": content, "status": "ok"})
    }
}

/// The stored text format.
const FORMAT: &str = "markdown";

fn search_descriptor() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "web_search",
        TOOLSET,
        "Search the web. The query goes to the search backend verbatim, so operators \
         such as site:, filetype:, intitle:, -term and quoted phrases work where the \
         backend supports them. Returns ranked results with title, url and snippet.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "The search query."},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_RESULTS, "description": "Most results (default 5)."}
            },
            "required": ["query"],
            "additionalProperties": false
        }),
    );
    web_traits(&mut d);
    d
}

fn extract_descriptor() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "web_extract",
        TOOLSET,
        "Fetch up to 5 web pages as Markdown (images become [IMAGE: alt] placeholders). \
         Each URL gets its own status; a failing URL does not fail the others. Private \
         network and cloud metadata addresses are refused.",
        json!({
            "type": "object",
            "properties": {
                "urls": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": MAX_URLS, "description": "http(s) URLs."},
                "char_limit": {"type": "integer", "minimum": CHAR_LIMIT_RANGE.0, "description": "Characters per page (default web.extract_char_limit)."}
            },
            "required": ["urls"],
            "additionalProperties": false
        }),
    );
    web_traits(&mut d);
    d
}

fn web_traits(descriptor: &mut ToolDescriptor) {
    descriptor.effect = Effect::None;
    descriptor.idempotent = true;
    descriptor.claims = Some(ResourceClaims::none());
    descriptor.capabilities = vec![Cap::parse("net.connect:*").expect("valid capability")];
}
