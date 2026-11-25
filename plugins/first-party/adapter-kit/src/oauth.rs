//! OAuth 2.0 access tokens a plugin obtains for itself and caches until
//! shortly before they expire: the client-credentials grant (Entra ID for
//! the Bot Connector and Microsoft Graph) and the JWT bearer grant
//! (RFC 7523, Google service accounts).

use serde_json::Value;

use crate::{clock, net, text};

/// Renew this long before a token expires.
const MARGIN_SECS: u64 = 300;

/// A cached access token.
#[derive(Debug, Clone, Default)]
pub struct Cached {
    /// The token and when (plugin clock ms) to renew it.
    token: Option<(String, u64)>,
}

impl Cached {
    /// The cached token, while fresh.
    #[must_use]
    pub fn fresh(&self) -> Option<&str> {
        self.token
            .as_ref()
            .filter(|(_, renew_at)| clock::now_ms() < *renew_at)
            .map(|(t, _)| t.as_str())
    }

    /// Posts `form` to `url` and keeps the `access_token` it answers.
    ///
    /// # Errors
    ///
    /// The endpoint refused (classified like a send) or answered no token.
    pub fn fetch(&mut self, url: &str, form: &[(&str, &str)]) -> Result<String, String> {
        let response = net::send(
            "POST",
            url,
            &[("content-type", "application/x-www-form-urlencoded")],
            Some(text::form_encode(form).into_bytes()),
        )?;
        net::classify(&response)?;
        let body: Value = response.json();
        let token = body["access_token"]
            .as_str()
            .ok_or("server: the token answer holds no access_token")?
            .to_owned();
        let lifetime = body["expires_in"].as_u64().unwrap_or(3600);
        let renew_at = clock::now_ms() + lifetime.saturating_sub(MARGIN_SECS) * 1000;
        self.token = Some((token.clone(), renew_at));
        Ok(token)
    }
}

/// An app's client-credentials token for one scope.
#[derive(Debug, Clone)]
pub struct ClientCredentials {
    url: String,
    client_id: String,
    secret: String,
    scope: String,
    cached: Cached,
}

impl ClientCredentials {
    /// The token of `client_id` (its secret in the secret named `secret`)
    /// from `url`, for `scope`.
    #[must_use]
    pub fn new(url: &str, client_id: &str, secret: &str, scope: &str) -> Self {
        Self {
            url: url.to_owned(),
            client_id: client_id.to_owned(),
            secret: secret.to_owned(),
            scope: scope.to_owned(),
            cached: Cached::default(),
        }
    }

    /// `Bearer <token>`.
    ///
    /// # Errors
    ///
    /// The secret is not set or the endpoint refused.
    pub fn bearer(&mut self) -> Result<String, String> {
        if let Some(token) = self.cached.fresh() {
            return Ok(format!("Bearer {token}"));
        }
        let secret = crate::host::secret(&self.secret)
            .ok_or_else(|| format!("permanent: secret {} is not set", self.secret))?;
        let token = self.cached.fetch(
            &self.url,
            &[
                ("grant_type", "client_credentials"),
                ("client_id", &self.client_id),
                ("client_secret", &secret),
                ("scope", &self.scope),
            ],
        )?;
        Ok(format!("Bearer {token}"))
    }
}
