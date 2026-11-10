//! Web result caching (spec: web-and-media-tools, Web result caching).
//!
//! Searches are memoized in-process, shared by every session and subagent
//! of the profile in this process, keyed by the normalized query, the
//! backend and the limit bucket; identical searches in flight at once
//! share one backend request. Extractions are kept in the profile's
//! `<profile>/cache/web/` store, so every process of the profile sees
//! them. Only successes are cached, and local, private and exempted hosts
//! never are.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pmt_config::schema::WebConfig;
use pmt_core::private_fs;
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;
use tokio::time::Instant;

use super::FullTextStore;
use super::backend::{Hit, Page};

/// Bounds of `web.cache_ttl_secs`.
pub const TTL_RANGE: (u64, u64) = (60, 86_400);

/// The limit buckets searches are asked and cached at.
pub const BUCKETS: [usize; 4] = [10, 20, 50, 100];

/// The cache lifetime per `config` (`None`: caching off).
#[must_use]
pub fn ttl(config: &WebConfig) -> Option<Duration> {
    config
        .cache_enabled
        .then(|| Duration::from_secs(config.cache_ttl_secs.clamp(TTL_RANGE.0, TTL_RANGE.1)))
}

/// The bucket a search of `limit` results is asked at.
#[must_use]
pub fn bucket(limit: usize) -> usize {
    BUCKETS
        .iter()
        .copied()
        .find(|b| limit <= *b)
        .unwrap_or(BUCKETS[BUCKETS.len() - 1])
}

/// A query compared case- and whitespace-insensitively.
#[must_use]
pub fn normalize(query: &str) -> String {
    query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// A search answer as cached: the hits and who answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Searched {
    /// The hits at the bucket's size.
    pub hits: Vec<Hit>,
    /// The `backend` the result reports.
    pub backend: String,
    /// Its billing source.
    pub billing_source: String,
    /// Answered by the keyless ring after the backend failed.
    pub rescued: bool,
}

type Key = (PathBuf, String, String, usize);

/// One memo slot: empty until a search succeeds, then the answer and when
/// it arrived.
type Slot = Arc<OnceCell<(Searched, Instant)>>;

/// The process-wide search memo.
static SEARCHES: LazyLock<Mutex<HashMap<Key, Slot>>> = LazyLock::new(Mutex::default);

/// A search through the memo of the profile whose cache directory is
/// `profile`: a fresh cached answer, the answer of an identical search in
/// flight, or `run`'s (cached only when it succeeds). The flag says
/// whether the answer was cached.
///
/// # Errors
///
/// `run`'s failure.
pub async fn search<F, Fut, E>(
    profile: &Path,
    backend: &str,
    query: &str,
    limit: usize,
    ttl: Duration,
    run: F,
) -> Result<(Searched, bool), E>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Searched, E>>,
{
    let key = (
        profile.to_path_buf(),
        backend.to_owned(),
        normalize(query),
        bucket(limit),
    );
    let fresh = |slot: &Slot| slot.get().is_none_or(|(_, at)| at.elapsed() < ttl);
    let cell = {
        let mut map = SEARCHES.lock().unwrap_or_else(PoisonError::into_inner);
        map.retain(|_, slot| fresh(slot));
        Arc::clone(map.entry(key).or_default())
    };
    if let Some((answer, _)) = cell.get() {
        return Ok((answer.clone(), true));
    }
    let mut ran = false;
    let (answer, _) = cell
        .get_or_try_init(|| {
            ran = true;
            async { run().await.map(|answer| (answer, Instant::now())) }
        })
        .await?;
    Ok((answer.clone(), !ran))
}

/// Whether results of `url` may be cached: never for local, private,
/// single-label and exempted hosts.
#[must_use]
pub fn cacheable(url: &str, exempt: &[String]) -> bool {
    let Some(host) = url::Url::parse(url).ok().and_then(|u| {
        u.host_str()
            .map(|h| h.trim_matches(['[', ']']).to_lowercase())
    }) else {
        return false;
    };
    if let Ok(ip) = host.parse::<IpAddr>() {
        return !local_ip(ip);
    }
    let last = host.rsplit('.').next().unwrap_or_default();
    let local = host == "localhost" || matches!(last, "localhost" | "local") || !host.contains('.');
    !local && !exempt.iter().any(|pattern| matches_host(&host, pattern))
}

fn local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| local_ip(IpAddr::V4(v4)))
        }
    }
}

/// `host` matches an exempt pattern: exact, `*.` wildcard (subdomains) or
/// a domain suffix (the domain and its subdomains).
fn matches_host(host: &str, pattern: &str) -> bool {
    let pattern = pattern.trim().trim_start_matches('.').to_lowercase();
    if let Some(domain) = pattern.strip_prefix("*.") {
        return host.ends_with(&format!(".{domain}"));
    }
    host == pattern || host.ends_with(&format!(".{pattern}"))
}

/// One extraction as stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored {
    url: String,
    backend: String,
    billing_source: String,
    title: String,
    markdown: String,
    at: u64,
}

/// A cached extraction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extracted {
    /// The page.
    pub page: Page,
    /// The backend that extracted it.
    pub backend: String,
    /// Its billing source.
    pub billing_source: String,
}

/// The extraction cache of a profile, in its full-text store directory.
#[derive(Debug, Clone)]
pub struct ExtractCache {
    store: FullTextStore,
}

const FORMAT: &str = "markdown";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl ExtractCache {
    /// The cache beside `store`'s texts.
    #[must_use]
    pub fn new(store: FullTextStore) -> Self {
        Self { store }
    }

    fn path(&self, url: &str, backend: &str) -> PathBuf {
        self.store
            .path_of(url, FORMAT, backend)
            .with_extension("json")
    }

    /// The fresh entry of (`url`, `backend`), if any.
    #[must_use]
    pub fn get(&self, url: &str, backend: &str, ttl: Duration) -> Option<Extracted> {
        let text = std::fs::read_to_string(self.path(url, backend)).ok()?;
        let stored: Stored = serde_json::from_str(&text).ok()?;
        let fresh = now().saturating_sub(stored.at) < ttl.as_secs();
        (fresh && stored.url == url).then_some(Extracted {
            page: Page {
                title: stored.title,
                markdown: stored.markdown,
            },
            backend: stored.backend,
            billing_source: stored.billing_source,
        })
    }

    /// Keeps a successful extraction of the requested `url`.
    pub fn put(&self, url: &str, lookup_backend: &str, extracted: &Extracted) {
        let stored = Stored {
            url: url.to_owned(),
            backend: extracted.backend.clone(),
            billing_source: extracted.billing_source.clone(),
            title: extracted.page.title.clone(),
            markdown: extracted.page.markdown.clone(),
            at: now(),
        };
        let written = private_fs::create_dir(self.store.dir()).and_then(|()| {
            let body = serde_json::to_vec(&stored).unwrap_or_default();
            private_fs::write_atomic(&self.path(url, lookup_backend), &body)
        });
        if let Err(err) = written {
            tracing::debug!(%err, "web extraction not cached");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_private_and_exempt_hosts_are_never_cached() {
        let exempt = vec!["*.ngrok-free.app".to_owned(), "example.org".to_owned()];
        for url in [
            "http://localhost:3000/",
            "http://printer.local/",
            "http://intranet/",
            "http://127.0.0.1/",
            "http://10.0.0.2/",
            "http://192.168.1.1/",
            "http://169.254.1.1/",
            "http://100.64.0.1/",
            "http://[::1]/",
            "http://[fd00::1]/",
            "https://abc.ngrok-free.app/",
            "https://example.org/",
            "https://docs.example.org/",
        ] {
            assert!(!cacheable(url, &exempt), "{url}");
        }
        for url in [
            "https://ngrok-free.app/",
            "https://rust-lang.org/",
            "http://93.184.216.34/",
        ] {
            assert!(cacheable(url, &exempt), "{url}");
        }
    }

    #[test]
    fn limits_round_up_to_buckets_and_queries_normalize() {
        assert_eq!(bucket(3), 10);
        assert_eq!(bucket(11), 20);
        assert_eq!(bucket(50), 50);
        assert_eq!(bucket(100), 100);
        assert_eq!(normalize("  Rust   ASYNC "), "rust async");
    }

    #[test]
    fn the_ttl_is_clamped_and_off_when_disabled() {
        let mut config = WebConfig {
            cache_ttl_secs: 5,
            ..WebConfig::default()
        };
        assert_eq!(ttl(&config), Some(Duration::from_mins(1)));
        config.cache_ttl_secs = 999_999;
        assert_eq!(ttl(&config), Some(Duration::from_hours(24)));
        config.cache_enabled = false;
        assert_eq!(ttl(&config), None);
    }
}
