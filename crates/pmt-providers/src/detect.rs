//! Endpoint auto-detection for multi-region key providers (spec:
//! model-providers, Endpoint auto-detection for multi-region key
//! providers).
//!
//! A `zai` key works on only some of Z.AI's four endpoints (international
//! or mainland, general or coding plan). With no `base_url` configured the
//! first session probes all four in parallel with a 1-token chat request
//! and keeps the highest-priority endpoint that answered HTTP 200. The
//! winner is remembered in profile state under a SHA-256 prefix of the key
//! (never the key), so later sessions do not probe; a probe set that found
//! nothing is remembered for [`FAILED_TTL`] so pickers opened in a row do
//! not repeat it. A `kimi-coding` key starting `sk-kimi-` belongs to Kimi
//! Code, served at [`KIMI_CODING_URL`] over Anthropic Messages; that needs
//! no probe.

use std::time::Duration;

use jiff::Timestamp;
use pmt_core::secret::SecretString;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::pool::PoolState;

/// Z.AI endpoints in priority order.
pub const ZAI_ENDPOINTS: [&str; 4] = [
    "https://api.z.ai/api/paas/v4",
    "https://open.bigmodel.cn/api/paas/v4",
    "https://api.z.ai/api/coding/paas/v4",
    "https://open.bigmodel.cn/api/coding/paas/v4",
];

/// The model a probe asks for (every Z.AI plan serves it).
pub const ZAI_PROBE_MODEL: &str = "glm-4.5-air";

/// Time limit of one probe.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// How long a probe set that found no endpoint is not repeated.
pub const FAILED_TTL: Duration = Duration::from_mins(5);

/// Kimi Code's host.
pub const KIMI_CODING_HOST: &str = "api.kimi.com";

/// Kimi Code's endpoint (Anthropic Messages).
pub const KIMI_CODING_URL: &str = "https://api.kimi.com/coding";

/// The key prefix of Kimi Code keys.
pub const KIMI_CODING_PREFIX: &str = "sk-kimi-";

/// What detection found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detected {
    /// Probed now.
    Probed(String),
    /// Remembered from an earlier probe.
    Remembered(String),
    /// No endpoint answered (now, or within [`FAILED_TTL`]): the provider
    /// default is used.
    Failed {
        /// The probe set ran now (not a remembered failure).
        probed: bool,
    },
}

impl Detected {
    /// The detected endpoint.
    #[must_use]
    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Probed(url) | Self::Remembered(url) => Some(url),
            Self::Failed { .. } => None,
        }
    }

    /// How the trace of `prometheus model explain` words it.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Probed(url) => format!("{url} (detected: the key's endpoint answered a probe)"),
            Self::Remembered(url) => format!("{url} (detected earlier for this key)"),
            Self::Failed { probed: true } => {
                "no endpoint accepted the key; using the default".to_owned()
            }
            Self::Failed { probed: false } => format!(
                "no endpoint accepted the key within the last {} s; using the default",
                FAILED_TTL.as_secs()
            ),
        }
    }
}

/// The Kimi endpoint of `key`: [`KIMI_CODING_URL`] for a Kimi Code key.
#[must_use]
pub fn kimi_endpoint(key: &SecretString) -> Option<&'static str> {
    key.expose()
        .starts_with(KIMI_CODING_PREFIX)
        .then_some(KIMI_CODING_URL)
}

/// The profile-state key remembering `provider`'s endpoint for `key`.
#[must_use]
pub fn memo_key(provider: &str, key: &SecretString) -> String {
    let digest = Sha256::digest(key.expose().as_bytes());
    let hex = crate::request::hex(&digest[..8]);
    format!("endpoint.{provider}.{hex}")
}

const FAILED_PREFIX: &str = "failed:";

/// Probes multi-region endpoints.
#[derive(Debug, Clone)]
pub struct Detector {
    client: reqwest::Client,
    candidates: Vec<String>,
    timeout: Duration,
}

impl Detector {
    /// Probes Z.AI's endpoints over `client`.
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            candidates: ZAI_ENDPOINTS.iter().map(|s| (*s).to_owned()).collect(),
            timeout: PROBE_TIMEOUT,
        }
    }

    /// Probes `candidates` (priority order) instead (tests).
    #[must_use]
    pub fn with_candidates(mut self, candidates: Vec<String>) -> Self {
        self.candidates = candidates;
        self
    }

    /// The `zai` endpoint of `key`, remembered in `state` when there is
    /// one.
    pub async fn zai(&self, key: &SecretString, state: Option<&dyn PoolState>) -> Detected {
        let memo = memo_key("zai", key);
        let now = Timestamp::now();
        if let Some(found) = state.and_then(|s| s.remembered(&memo).ok().flatten()) {
            match recalled(&found, now) {
                Some(Detected::Failed { .. }) => return Detected::Failed { probed: false },
                Some(hit) => return hit,
                None => {}
            }
        }
        let detected = self.probe_all(key).await;
        let value = match &detected {
            Detected::Probed(url) => url.clone(),
            _ => format!("{FAILED_PREFIX}{}", now.as_second()),
        };
        if let Some(state) = state
            && let Err(err) = state.remember(&memo, &value)
        {
            tracing::warn!(code = err.code().code, "endpoint detection not remembered");
        }
        detected
    }

    /// Runs every probe at once and keeps the highest-priority endpoint
    /// that answered 200, decided as soon as every probe above it ended.
    async fn probe_all(&self, key: &SecretString) -> Detected {
        let mut probes: Vec<_> = self
            .candidates
            .iter()
            .map(|url| {
                let (client, url, key, timeout) =
                    (self.client.clone(), url.clone(), key.clone(), self.timeout);
                tokio::spawn(
                    async move { probe(&client, &url, &key, timeout).await.then_some(url) },
                )
            })
            .collect();
        let mut found = None;
        for handle in &mut probes {
            if let Ok(Some(url)) = handle.await {
                found = Some(url);
                break;
            }
        }
        for handle in probes {
            handle.abort();
        }
        found.map_or(Detected::Failed { probed: true }, Detected::Probed)
    }
}

/// A remembered value: an endpoint, a failure still within
/// [`FAILED_TTL`], or `None` (an expired failure: probe again).
fn recalled(value: &str, now: Timestamp) -> Option<Detected> {
    let Some(at) = value.strip_prefix(FAILED_PREFIX) else {
        return Some(Detected::Remembered(value.to_owned()));
    };
    let at = at.parse::<i64>().ok()?;
    let ttl = i64::try_from(FAILED_TTL.as_secs()).unwrap_or(i64::MAX);
    (now.as_second().saturating_sub(at) < ttl).then_some(Detected::Failed { probed: false })
}

/// Whether `base` answers a 1-token chat request with HTTP 200.
async fn probe(
    client: &reqwest::Client,
    base: &str,
    key: &SecretString,
    timeout: Duration,
) -> bool {
    let body = json!({
        "model": ZAI_PROBE_MODEL,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1,
        "stream": false,
    });
    let request = client
        .post(format!("{}/chat/completions", base.trim_end_matches('/')))
        .bearer_auth(key.expose())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .timeout(timeout)
        .send();
    matches!(request.await, Ok(response) if response.status() == reqwest::StatusCode::OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_memo_key_never_holds_the_key() {
        let key = SecretString::new("glm-secret-value");
        let memo = memo_key("zai", &key);
        assert!(memo.starts_with("endpoint.zai."));
        assert!(!memo.contains("secret"));
        assert_eq!(
            memo,
            memo_key("zai", &SecretString::new("glm-secret-value"))
        );
        assert_ne!(memo, memo_key("zai", &SecretString::new("other")));
    }

    #[test]
    fn a_failure_is_remembered_for_its_ttl_only() {
        let now = Timestamp::from_second(10_000).unwrap();
        assert_eq!(
            recalled("failed:9800", now),
            Some(Detected::Failed { probed: false })
        );
        assert_eq!(recalled("failed:9600", now), None);
        assert_eq!(
            recalled("https://x.test/v4", now),
            Some(Detected::Remembered("https://x.test/v4".into()))
        );
    }

    #[test]
    fn kimi_code_keys_go_to_the_coding_endpoint() {
        assert_eq!(
            kimi_endpoint(&SecretString::new("sk-kimi-abc")),
            Some(KIMI_CODING_URL)
        );
        assert_eq!(kimi_endpoint(&SecretString::new("sk-moonshot")), None);
    }
}
