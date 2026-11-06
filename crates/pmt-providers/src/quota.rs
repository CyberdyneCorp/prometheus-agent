//! Subscription plan and quota visibility (spec: model-providers,
//! Subscription plan and quota visibility).
//!
//! Subscriptions whose provider exposes a usage endpoint (ChatGPT plans,
//! Claude subscriptions) report a plan label and quota windows. The
//! newest probe of each credential is kept in `state.db` and refreshed at
//! most once per [`REFRESH`]; a window at 100% benches the credential
//! until that window resets, and a later probe showing the quota restored
//! early clears the bench. Other providers are shown as
//! [`NOT_REPORTED`].

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use pmt_store::Store;
use pmt_store::provider_state::QuotaRow;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How old a probe may get before the next one.
pub const REFRESH: Duration = Duration::from_mins(1);

/// What a provider without quota data shows.
pub const NOT_REPORTED: &str = "quota: not reported by provider";

/// The bench reason of an exhausted quota window.
pub const BENCH_REASON: &str = "quota";

/// One quota window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// `5h`, `weekly`...
    pub name: String,
    /// Used share, 0-100.
    pub used_percent: f64,
    /// When the window resets (RFC 3339).
    pub resets_at: Option<String>,
}

impl Window {
    fn reset(&self) -> Option<Timestamp> {
        self.resets_at.as_deref()?.parse().ok()
    }
}

/// A credential's plan and quota windows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Quota {
    /// The plan label.
    pub plan: Option<String>,
    /// The windows.
    pub windows: Vec<Window>,
    /// When it was probed (RFC 3339).
    pub fetched_at: String,
}

impl Quota {
    /// When an exhausted window (100% used) resets: the latest such reset,
    /// or an hour from `now` when an exhausted window names none. A window
    /// whose reset time has passed has reset.
    #[must_use]
    pub fn exhausted_until(&self, now: Timestamp) -> Option<Timestamp> {
        self.windows
            .iter()
            .filter(|w| w.used_percent >= 100.0)
            .filter_map(|w| match w.reset() {
                Some(reset) => (reset > now).then_some(reset),
                None => Some(now + jiff::SignedDuration::from_hours(1)),
            })
            .max()
    }

    /// `plan plus · 5h 72% (resets 2026-11-06T14:00:00Z) · weekly 30%`.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut text = format!("plan {}", self.plan.as_deref().unwrap_or("unknown"));
        for w in &self.windows {
            let _ = write!(text, " · {} {:.0}%", w.name, w.used_percent);
            if let Some(reset) = &w.resets_at {
                let _ = write!(text, " (resets {reset})");
            }
        }
        text
    }
}

/// The usage endpoint of `provider` whose requests go to `base_url`.
#[must_use]
pub fn endpoint(provider: &str, base_url: &str) -> Option<String> {
    let base = base_url.trim_end_matches('/');
    match provider {
        "openai-codex" => Some(format!(
            "{}/wham/usage",
            base.strip_suffix("/codex").unwrap_or(base)
        )),
        "anthropic" => Some(format!("{base}/api/oauth/usage")),
        _ => None,
    }
}

/// Probes `provider`'s usage endpoint with the subscription `token`.
///
/// # Errors
///
/// `provider.call_failed` for a failed or unparsable answer.
pub async fn probe(
    client: &reqwest::Client,
    provider: &str,
    base_url: &str,
    token: &SecretString,
    account_id: Option<&str>,
) -> Result<Quota, Error> {
    let url = endpoint(provider, base_url).ok_or_else(|| failed(provider, "no usage endpoint"))?;
    let mut request = client
        .get(&url)
        .bearer_auth(token.expose())
        .timeout(Duration::from_secs(10));
    if provider == "anthropic" {
        request = request.header("anthropic-beta", "oauth-2025-04-20");
    }
    if let Some(account) = account_id {
        request = request.header("ChatGPT-Account-ID", account);
    }
    let response = request
        .send()
        .await
        .map_err(|e| failed(provider, &e.to_string()))?;
    if !response.status().is_success() {
        return Err(failed(provider, &format!("HTTP {}", response.status())));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| failed(provider, &e.to_string()))?;
    let body: Value =
        serde_json::from_slice(&bytes).map_err(|e| failed(provider, &e.to_string()))?;
    let (plan, windows) = match provider {
        "openai-codex" => codex(&body),
        _ => anthropic(&body),
    };
    Ok(Quota {
        plan,
        windows,
        fetched_at: Timestamp::now().to_string(),
    })
}

fn failed(provider: &str, why: &str) -> Error {
    Error::new(
        &codes::PROVIDER_CALL_FAILED,
        format!("{provider}: usage probe failed: {why}"),
    )
}

/// ChatGPT's `wham/usage`: `plan_type` and `rate_limit.primary_window` /
/// `secondary_window` with `used_percent`, `limit_window_seconds` and
/// `reset_at` (epoch seconds).
fn codex(body: &Value) -> (Option<String>, Vec<Window>) {
    let plan = body["plan_type"].as_str().map(str::to_owned);
    let mut windows = Vec::new();
    for key in ["primary_window", "secondary_window"] {
        let w = &body["rate_limit"][key];
        let Some(used) = w["used_percent"].as_f64() else {
            continue;
        };
        let name = match w["limit_window_seconds"].as_u64() {
            Some(secs) if secs >= 6 * 86_400 => "weekly".to_owned(),
            Some(secs) if secs % 3600 == 0 => format!("{}h", secs / 3600),
            Some(secs) => format!("{secs}s"),
            None => key.trim_end_matches("_window").to_owned(),
        };
        let resets_at = w["reset_at"]
            .as_i64()
            .and_then(|s| Timestamp::from_second(s).ok())
            .map(|t| t.to_string());
        windows.push(Window {
            name,
            used_percent: used,
            resets_at,
        });
    }
    (plan, windows)
}

/// Claude's `oauth/usage`: `five_hour`, `seven_day` (and model-specific
/// weekly windows) with `utilization` and `resets_at`.
fn anthropic(body: &Value) -> (Option<String>, Vec<Window>) {
    let names = [
        ("five_hour", "5h"),
        ("seven_day", "weekly"),
        ("seven_day_opus", "weekly opus"),
        ("seven_day_sonnet", "weekly sonnet"),
    ];
    let windows = names
        .iter()
        .filter_map(|(key, name)| {
            let w = &body[*key];
            Some(Window {
                name: (*name).to_owned(),
                used_percent: w["utilization"].as_f64()?,
                resets_at: w["resets_at"].as_str().map(str::to_owned),
            })
        })
        .collect();
    let plan = body["plan"]
        .as_str()
        .or_else(|| body["subscription_type"].as_str())
        .map(str::to_owned);
    (plan, windows)
}

/// The stored snapshot of `credential` of `provider` in `store`.
#[must_use]
pub fn stored(store: &Store, provider: &str, credential: &str) -> Option<Quota> {
    let row = store.subscription_quota(provider, credential).ok()??;
    Some(Quota {
        plan: row.plan,
        windows: serde_json::from_str(&row.windows).ok()?,
        fetched_at: row.fetched_at.to_string(),
    })
}

/// What a session billing `billing_source` on `provider` shows about its
/// subscription: `None` when the source is not a subscription;
/// `Some((false, None))` when the provider reports no quota;
/// `Some((true, snapshot))` otherwise.
#[must_use]
pub fn for_session(
    store: &Store,
    provider: &str,
    billing_source: &str,
) -> Option<(bool, Option<Quota>)> {
    let rest = billing_source.strip_prefix("subscription:")?;
    let label = rest.split_once('/').map_or(rest, |(_, label)| label);
    if endpoint(provider, "").is_none() {
        return Some((false, None));
    }
    let id = crate::pool::subscription_id(label);
    Some((true, stored(store, provider, &id)))
}

/// Quota snapshots of a profile's subscription credentials: read from
/// `state.db`, probed when older than [`REFRESH`] (a failed probe is not
/// retried before then either).
pub struct QuotaTracker {
    client: reqwest::Client,
    store: Arc<Store>,
    refresh: Duration,
    attempts: Mutex<BTreeMap<(String, String), Timestamp>>,
}

impl std::fmt::Debug for QuotaTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuotaTracker").finish_non_exhaustive()
    }
}

/// What a probe of one credential needs.
#[derive(Debug, Clone, Copy)]
pub struct Probe<'a> {
    /// Provider id.
    pub provider: &'a str,
    /// Credential id (`subscription:<label>`).
    pub credential: &'a str,
    /// The route's base URL.
    pub base_url: &'a str,
    /// The access token.
    pub token: &'a SecretString,
    /// The account id, when the provider routes by it.
    pub account_id: Option<&'a str>,
}

impl QuotaTracker {
    /// Snapshots in `store`, probed with `client`.
    #[must_use]
    pub fn new(client: reqwest::Client, store: Arc<Store>) -> Self {
        Self {
            client,
            store,
            refresh: REFRESH,
            attempts: Mutex::new(BTreeMap::new()),
        }
    }

    /// Probes again once a snapshot is `refresh` old (tests shorten the
    /// minute).
    #[must_use]
    pub fn with_refresh(mut self, refresh: Duration) -> Self {
        self.refresh = refresh;
        self
    }

    /// The stored snapshot of `credential` of `provider`, however old.
    #[must_use]
    pub fn stored(&self, provider: &str, credential: &str) -> Option<Quota> {
        stored(&self.store, provider, credential)
    }

    /// The snapshot of `probe`'s credential, probed first when the stored
    /// one is older than [`REFRESH`] and no probe was tried since; `None`
    /// when the provider reports no quota or nothing was ever fetched.
    pub async fn current(&self, probe: Probe<'_>) -> Option<Quota> {
        endpoint(probe.provider, probe.base_url)?;
        let now = Timestamp::now();
        let fresh = |at: Timestamp| now.duration_since(at).unsigned_abs() < self.refresh;
        let stored = self
            .store
            .subscription_quota(probe.provider, probe.credential);
        if let Ok(Some(row)) = &stored
            && fresh(row.fetched_at)
        {
            return self.stored(probe.provider, probe.credential);
        }
        let key = (probe.provider.to_owned(), probe.credential.to_owned());
        {
            let mut attempts = self
                .attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if attempts.get(&key).is_some_and(|at| fresh(*at)) {
                return self.stored(probe.provider, probe.credential);
            }
            attempts.insert(key, now);
        }
        match self::probe(
            &self.client,
            probe.provider,
            probe.base_url,
            probe.token,
            probe.account_id,
        )
        .await
        {
            Ok(quota) => {
                self.save(probe.provider, probe.credential, &quota, now);
                Some(quota)
            }
            Err(err) => {
                tracing::debug!(error = %err, "subscription usage probe failed");
                self.stored(probe.provider, probe.credential)
            }
        }
    }

    fn save(&self, provider: &str, credential: &str, quota: &Quota, now: Timestamp) {
        let row = QuotaRow {
            fetched_at: now,
            plan: quota.plan.clone(),
            windows: serde_json::to_string(&quota.windows).unwrap_or_else(|_| "[]".to_owned()),
        };
        if let Err(err) = self
            .store
            .put_subscription_quota(provider, credential, &row)
        {
            tracing::warn!(error = %err, "cannot store a quota snapshot");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_windows_are_named_by_their_length() {
        let body = serde_json::json!({
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": {"used_percent": 72.0, "limit_window_seconds": 18000, "reset_at": 1_800_000_000},
                "secondary_window": {"used_percent": 30.0, "limit_window_seconds": 604_800}
            }
        });
        let (plan, windows) = codex(&body);
        assert_eq!(plan.as_deref(), Some("pro"));
        assert_eq!(windows[0].name, "5h");
        assert_eq!(windows[1].name, "weekly");
        assert!(windows[1].resets_at.is_none());
    }

    #[test]
    fn an_exhausted_window_benches_until_its_reset() {
        let now = Timestamp::now();
        let reset = now + jiff::SignedDuration::from_mins(40);
        let quota = Quota {
            plan: None,
            windows: vec![
                Window {
                    name: "5h".to_owned(),
                    used_percent: 100.0,
                    resets_at: Some(reset.to_string()),
                },
                Window {
                    name: "weekly".to_owned(),
                    used_percent: 50.0,
                    resets_at: None,
                },
            ],
            fetched_at: now.to_string(),
        };
        assert_eq!(quota.exhausted_until(now), Some(reset));
        assert_eq!(
            endpoint("openai-codex", "https://chatgpt.com/backend-api/codex").as_deref(),
            Some("https://chatgpt.com/backend-api/wham/usage")
        );
        assert_eq!(endpoint("nous", "https://x"), None);
    }

    #[test]
    fn subscriptions_without_a_usage_endpoint_are_not_reported() {
        let dir =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        let home = pmt_config::Home::open(dir.path()).unwrap();
        let profile = home
            .profile(&pmt_config::ProfileName::default_profile())
            .unwrap();
        let scope = pmt_config::ProfileScope::open(
            profile,
            &pmt_config::Overrides::default(),
            pmt_config::Launch::Other,
        )
        .unwrap();
        let store = Store::open(&scope).unwrap();
        assert_eq!(
            for_session(&store, "xai-oauth", "subscription:xai-oauth/default"),
            Some((false, None))
        );
        assert_eq!(
            for_session(&store, "openai-codex", "subscription:openai-codex/p"),
            Some((true, None))
        );
        assert_eq!(
            for_session(&store, "openrouter", "api_key:openrouter"),
            None
        );
    }
}
