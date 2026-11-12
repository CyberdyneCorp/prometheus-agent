//! Sign-in tokens exchanged for short-lived API tokens (spec:
//! model-providers, Hermes model-provider plugins hosted by the Python
//! bridge): GitHub Copilot signs in with GitHub's device code, and every
//! request carries a Copilot API token obtained by presenting the GitHub
//! token to the sign-in's `exchange_url`. The answer
//! (`{"token", "expires_at", "refresh_in"}`) is cached per sign-in and
//! exchanged again a minute before it expires (or once `refresh_in` has
//! passed); a request refused with HTTP 401 drops it so the retry
//! exchanges anew. The plugin serving the provider never sees either
//! token's source: it gets the request the host built.

use std::collections::BTreeMap;
use std::sync::Mutex;

use jiff::{SignedDuration, Timestamp};
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use serde_json::Value;

/// Tokens are exchanged again this long before they expire.
const EARLY: SignedDuration = SignedDuration::from_secs(60);
/// Lifetime assumed when the answer gives none.
const DEFAULT_LIFETIME: SignedDuration = SignedDuration::from_mins(25);

/// One exchanged token and when it stops being used.
#[derive(Clone)]
struct Exchanged {
    token: SecretString,
    use_until: Timestamp,
}

/// The exchanged tokens of a profile's sign-ins.
pub struct Exchanges {
    client: reqwest::Client,
    cache: Mutex<BTreeMap<String, Exchanged>>,
}

impl std::fmt::Debug for Exchanges {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Exchanges").finish_non_exhaustive()
    }
}

impl Exchanges {
    /// Exchanges with `client`.
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    fn key(provider: &str, label: &str) -> String {
        format!("{provider}/{label}")
    }

    /// The API token of sign-in `label` of `provider`, exchanged at `url`
    /// for its `sign_in` token when none is cached or it is about to
    /// expire.
    ///
    /// # Errors
    ///
    /// `auth.reauth_required` when the exchange refuses the sign-in token
    /// (401/403: sign in again); `provider.call_failed` for a transport
    /// error or another refusal.
    pub async fn token(
        &self,
        url: &str,
        provider: &str,
        label: &str,
        sign_in: &SecretString,
    ) -> Result<SecretString, Error> {
        let key = Self::key(provider, label);
        let now = Timestamp::now();
        if let Some(cached) = self.lock().get(&key)
            && cached.use_until > now
        {
            return Ok(cached.token.clone());
        }
        let exchanged = self.exchange(url, provider, label, sign_in, now).await?;
        let token = exchanged.token.clone();
        self.lock().insert(key, exchanged);
        Ok(token)
    }

    /// Drops the cached token of `provider`'s sign-in `label` (the API
    /// refused it).
    pub fn invalidate(&self, provider: &str, label: &str) {
        self.lock().remove(&Self::key(provider, label));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Exchanged>> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn exchange(
        &self,
        url: &str,
        provider: &str,
        label: &str,
        sign_in: &SecretString,
        now: Timestamp,
    ) -> Result<Exchanged, Error> {
        let unavailable = |why: String| {
            Error::new(
                &codes::PROVIDER_CALL_FAILED,
                format!("the {provider} token exchange failed: {why}"),
            )
        };
        let response = self
            .client
            .get(url)
            .header("authorization", format!("token {}", sign_in.expose()))
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| unavailable(e.to_string()))?;
        let status = response.status().as_u16();
        if status == 401 || status == 403 {
            return Err(Error::new(
                &codes::AUTH_REAUTH_REQUIRED,
                format!("{provider} refused the sign-in '{label}' (HTTP {status})"),
            )
            .with_hint(format!("sign in again: prometheus auth add {provider}")));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| unavailable(format!("unreadable answer: {e}")))?;
        let body: Value = serde_json::from_slice(&bytes)
            .map_err(|e| unavailable(format!("unreadable answer: {e}")))?;
        if !(200..300).contains(&status) {
            return Err(unavailable(format!("HTTP {status}")));
        }
        let token = body["token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| unavailable("the answer holds no token".to_owned()))?;
        Ok(Exchanged {
            token: SecretString::new(token.to_owned()),
            use_until: use_until(&body, now),
        })
    }
}

/// When an exchanged token stops being used: a minute before
/// `expires_at`, or at `refresh_in`, whichever comes first.
fn use_until(body: &Value, now: Timestamp) -> Timestamp {
    let expiry = body["expires_at"]
        .as_i64()
        .and_then(|at| Timestamp::from_second(at).ok())
        .map_or_else(
            || now + DEFAULT_LIFETIME,
            |at| at.checked_sub(EARLY).unwrap_or(at),
        );
    let refresh = body["refresh_in"]
        .as_i64()
        .map(|secs| now + SignedDuration::from_secs(secs));
    refresh.map_or(expiry, |refresh| refresh.min(expiry))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_token_is_used_until_a_minute_before_expiry_or_its_refresh_time() {
        let now = Timestamp::from_second(1_000_000).unwrap();
        let expires = json!({"expires_at": 1_001_800});
        assert_eq!(
            use_until(&expires, now),
            Timestamp::from_second(1_001_740).unwrap()
        );
        let sooner = json!({"expires_at": 1_001_800, "refresh_in": 1500});
        assert_eq!(
            use_until(&sooner, now),
            Timestamp::from_second(1_001_500).unwrap()
        );
        assert_eq!(use_until(&json!({}), now), now + DEFAULT_LIFETIME);
    }
}
