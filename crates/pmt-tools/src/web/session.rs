//! What one `web_search` or `web_extract` call does: the configured
//! source, the result caches, the one-shot keyless rescue, and the result
//! shape with its provider and billing source (spec: web-and-media-tools,
//! `web_search` / `web_extract` tool contracts; Keyless web search rotation
//! is explicit opt-in; Web result caching; Media tool credentials and
//! billing are explicit).

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use regex::Regex;
use serde_json::{Value, json};

use super::cache::{self, ExtractCache, Extracted, Searched};
use super::keyless::Ring;
use super::tools::{CHAR_LIMIT_RANGE, FORMAT};
use super::window::window;
use super::{Page, Source, WebBackend, WebSession};
use crate::result::{Status, ToolResult};
use crate::usage::{self, CACHED};

/// Markdown images: `![alt](src)`.
static IMAGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!\[([^\]]*)\]\([^)]*\)").expect("valid image pattern"));

/// Why a live search gave no cacheable answer.
enum Miss {
    /// It failed: `(backend, reason)`.
    Failed(String, String),
    /// The keyless rescue answered: returned, never cached, so the next
    /// call tries the configured backend again.
    Rescued(Searched),
}

impl Source {
    /// The id the result reports before a vendor answers.
    fn label(&self) -> String {
        match self {
            Self::Backend(backend) => backend.id().to_owned(),
            Self::Keyless(_) => super::keyless::KEYLESS.to_owned(),
            Self::Provider => "openai-native".to_owned(),
        }
    }
}

/// The keyless answer of a ring vendor.
fn keyless(vendor: &str) -> String {
    format!("keyless:{vendor}")
}

impl WebSession {
    /// Runs one search.
    pub async fn search(&self, query: &str, limit: usize) -> ToolResult {
        let source = match &self.search {
            Ok(source) => source,
            Err(why) => return ToolResult::error(format!("web_search unavailable: {why}")),
        };
        let label = source.label();
        let answer = match cache::ttl(&self.config) {
            Some(ttl) => {
                let asked = cache::bucket(limit);
                cache::search(self.full_text.dir(), &label, query, limit, ttl, || {
                    self.search_live(source, query, asked)
                })
                .await
            }
            None => self
                .search_live(source, query, limit)
                .await
                .map(|answer| (answer, false)),
        };
        let (answer, cached) = match answer {
            Ok(answer) => answer,
            Err(Miss::Rescued(answer)) => (answer, false),
            Err(Miss::Failed(backend, why)) => {
                return ToolResult::error(format!("web_search failed ({backend}): {why}"));
            }
        };
        search_result(answer, limit, cached)
    }

    async fn search_live(
        &self,
        source: &Source,
        query: &str,
        limit: usize,
    ) -> Result<Searched, Miss> {
        match source {
            Source::Backend(backend) => match backend.search(query, limit).await {
                Ok(hits) => Ok(Searched {
                    hits,
                    backend: backend.id().to_owned(),
                    billing_source: backend.billing_source(),
                    rescued: false,
                }),
                Err(why) => Err(self
                    .rescue_search(backend.as_ref(), why, query, limit)
                    .await),
            },
            Source::Keyless(ring) => ring
                .search(query, limit)
                .await
                .map(|(hits, vendor)| Searched {
                    hits,
                    backend: keyless(&vendor),
                    billing_source: keyless(&vendor),
                    rescued: false,
                })
                .map_err(|(vendor, why)| Miss::Failed(keyless(&vendor), why)),
            Source::Provider => Err(Miss::Failed(
                "openai-native".to_owned(),
                "the search runs on the model provider's Responses route, not as a local call"
                    .to_owned(),
            )),
        }
    }

    /// One retry of a failed keyed search on the keyless ring, when opted
    /// in.
    async fn rescue_search(
        &self,
        backend: &dyn WebBackend,
        why: String,
        query: &str,
        limit: usize,
    ) -> Miss {
        let Some(ring) = &self.rescue else {
            return Miss::Failed(backend.id().to_owned(), why);
        };
        match ring.search(query, limit).await {
            Ok((hits, vendor)) => Miss::Rescued(Searched {
                hits,
                backend: keyless(&vendor),
                billing_source: keyless(&vendor),
                rescued: true,
            }),
            Err((vendor, rescue)) => Miss::Failed(
                backend.id().to_owned(),
                format!("{why} (keyless rescue failed on {vendor}: {rescue})"),
            ),
        }
    }

    /// Extracts each URL; one failing URL never fails the others.
    pub async fn extract(&self, urls: &[String], char_limit: Option<usize>) -> ToolResult {
        let source = match &self.extract {
            Ok(Source::Provider) | Err(_) => {
                let why = self
                    .extract
                    .as_ref()
                    .err()
                    .map_or("search-only backend", String::as_str);
                return ToolResult::error(format!("web_extract unavailable: {why}"));
            }
            Ok(source) => source,
        };
        let budget = char_limit
            .unwrap_or_else(|| {
                usize::try_from(self.config.extract_char_limit).unwrap_or(usize::MAX)
            })
            .clamp(CHAR_LIMIT_RANGE.0, CHAR_LIMIT_RANGE.1);
        let mut results = Vec::with_capacity(urls.len());
        for url in urls {
            results.push(self.extract_one(source, url, budget).await);
        }
        extract_result(results, &source.label())
    }

    async fn extract_one(&self, source: &Source, url: &str, budget: usize) -> Value {
        if let Err(err) = self.fetcher.check(url).await {
            return json!({"url": url, "status": "blocked", "error": err.message()});
        }
        let label = source.label();
        let ttl = cache::ttl(&self.config)
            .filter(|_| cache::cacheable(url, &self.config.cache_exempt_hosts));
        let store = ExtractCache::new(self.full_text.clone());
        if let Some(hit) = ttl.and_then(|ttl| store.get(url, &label, ttl)) {
            return self.page_entry(&hit, url, budget, true);
        }
        let outcome = match self.extract_timeout() {
            Some(limit) => tokio::time::timeout(limit, self.extract_live(source, url))
                .await
                .ok(),
            None => Some(self.extract_live(source, url).await),
        };
        match outcome {
            None => json!({"url": url, "status": "timeout", "backend": label, "error": format!(
                "no answer within {} s (web.extract_timeout_secs)", self.config.extract_timeout_secs)}),
            Some(Err((backend, why))) => json!({"url": url, "status": "error", "backend": backend,
                "error": format!("web_extract failed ({backend}): {why}")}),
            Some(Ok((extracted, rescued))) => {
                if ttl.is_some() && !rescued {
                    store.put(url, &label, &extracted);
                }
                let mut entry = self.page_entry(&extracted, url, budget, false);
                if rescued {
                    entry["rescued"] = Value::Bool(true);
                }
                entry
            }
        }
    }

    async fn extract_live(
        &self,
        source: &Source,
        url: &str,
    ) -> Result<(Extracted, bool), (String, String)> {
        let ring = match source {
            Source::Keyless(ring) => {
                let (page, vendor) = ring
                    .extract(url)
                    .await
                    .map_err(|(vendor, why)| (keyless(&vendor), why))?;
                return Ok((ring_page(page, &vendor), false));
            }
            Source::Provider => return Err(("openai-native".to_owned(), "search-only".to_owned())),
            Source::Backend(backend) => match backend.extract(url).await {
                Ok(page) => {
                    let extracted = Extracted {
                        page,
                        backend: backend.id().to_owned(),
                        billing_source: backend.billing_source(),
                    };
                    return Ok((extracted, false));
                }
                Err(why) => match &self.rescue {
                    Some(ring) => (ring, backend.id().to_owned(), why),
                    None => return Err((backend.id().to_owned(), why)),
                },
            },
        };
        let (ring, backend, why) = ring;
        rescue_extract(ring, url, backend, why).await
    }

    /// The `web_extract` timeout per URL (`None`: disabled).
    fn extract_timeout(&self) -> Option<Duration> {
        let secs = self.config.extract_timeout_secs;
        (secs > 0).then(|| Duration::from_secs(secs))
    }

    /// The entry of an extracted page: whole within `budget`, else the
    /// head/tail window over the full text stored on disk. The text is the
    /// same whether it came from the cache or not (a repeat extraction is
    /// byte-identical); `cached` marks the result's metadata.
    fn page_entry(&self, extracted: &Extracted, url: &str, budget: usize, cached: bool) -> Value {
        let page = &extracted.page;
        let text = IMAGE.replace_all(&page.markdown, "[IMAGE: $1]");
        let content = if text.chars().count() <= budget {
            text.into_owned()
        } else {
            match self.full_text.save(url, FORMAT, &extracted.backend, &text) {
                Ok(stored) => window(&text, budget, &stored),
                Err(err) => {
                    return json!({"url": url, "status": "error", "error": format!(
                        "web_extract could not store the full text: {err}")});
                }
            }
        };
        let mut entry = json!({"url": url, "title": page.title, "content": content, "status": "ok",
            "backend": extracted.backend, "billing_source": extracted.billing_source});
        if cached {
            entry[CACHED] = Value::Bool(true);
        }
        entry
    }
}

fn ring_page(page: Page, vendor: &str) -> Extracted {
    Extracted {
        page,
        backend: keyless(vendor),
        billing_source: keyless(vendor),
    }
}

async fn rescue_extract(
    ring: &Arc<Ring>,
    url: &str,
    backend: String,
    why: String,
) -> Result<(Extracted, bool), (String, String)> {
    match ring.extract(url).await {
        Ok((page, vendor)) => Ok((ring_page(page, &vendor), true)),
        Err((vendor, rescue)) => Err((
            backend,
            format!("{why} (keyless rescue failed on {vendor}: {rescue})"),
        )),
    }
}

/// The `web_search` result of `answer`, at most `limit` hits.
fn search_result(answer: Searched, limit: usize, cached: bool) -> ToolResult {
    let results: Vec<Value> = answer
        .hits
        .into_iter()
        .take(limit)
        .enumerate()
        .map(|(index, hit)| {
            json!({"title": hit.title, "url": hit.url, "snippet": hit.snippet, "position": index + 1})
        })
        .collect();
    let mut body = json!({"results": results, "backend": answer.backend,
        "billing_source": answer.billing_source});
    if answer.rescued {
        body["rescued"] = Value::Bool(true);
    }
    let mut result = ToolResult::ok(body.to_string());
    usage::mark(&mut result, &answer.backend, "", &answer.billing_source);
    if cached {
        result.metadata.insert(CACHED.into(), Value::Bool(true));
    }
    result
}

/// The `web_extract` result of the per-URL `entries`: the first fetched
/// entry names the backend and billing (cached when every fetched entry
/// was).
fn extract_result(entries: Vec<Value>, label: &str) -> ToolResult {
    let fetched: Vec<&Value> = entries.iter().filter(|e| e["status"] == "ok").collect();
    let text =
        |entry: Option<&&Value>, key: &str| entry.and_then(|e| e[key].as_str()).map(str::to_owned);
    let backend = text(fetched.first(), "backend").unwrap_or_else(|| label.to_owned());
    let billing = text(fetched.first(), "billing_source").unwrap_or_default();
    let all_cached = !fetched.is_empty() && fetched.iter().all(|e| e[CACHED] == true);
    let status = if fetched.is_empty() {
        Status::Error
    } else {
        Status::Ok
    };
    let entries: Vec<Value> = entries
        .into_iter()
        .map(|mut entry| {
            if let Some(map) = entry.as_object_mut() {
                map.remove(CACHED);
            }
            entry
        })
        .collect();
    let mut result = ToolResult::text(
        status,
        json!({"results": entries, "backend": backend}).to_string(),
    );
    if !billing.is_empty() {
        usage::mark(&mut result, &backend, "", &billing);
    }
    if all_cached {
        result.metadata.insert(CACHED.into(), Value::Bool(true));
    }
    result
}
