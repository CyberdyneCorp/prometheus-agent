//! Requests to a sign-in's endpoints: form or JSON bodies, bounded retries
//! of transport failures, and the token response (spec: model-providers,
//! Device-code and browser PKCE sign-in flows). Bodies and tokens are
//! never logged.

use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use serde_json::{Map, Value};

use super::spec::{OAuthSpec, TokenEncoding};

/// Extra attempts after a transport failure or a 5xx answer of the
/// device-code request and the token exchange.
pub const RETRIES: u32 = 2;

/// Pause before a retry.
const RETRY_PAUSE: Duration = Duration::from_millis(500);

/// An endpoint's answer.
#[derive(Debug)]
pub struct Reply {
    /// HTTP status.
    pub status: u16,
    /// The JSON body (`Null` when it is not JSON).
    pub body: Value,
}

impl Reply {
    /// The string field `name` of the body.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&str> {
        self.body.get(name).and_then(Value::as_str)
    }

    /// `error` / `error_description` for messages (never token values).
    #[must_use]
    pub fn describe(&self) -> String {
        let error = self.field("error").unwrap_or("error");
        match self.field("error_description") {
            Some(text) => format!("HTTP {}: {error}: {text}", self.status),
            None => format!("HTTP {}: {error}", self.status),
        }
    }
}

/// Sends one POST with `fields`, as JSON when `json`, else form-encoded.
///
/// # Errors
///
/// The transport failure's text (no response was read).
pub async fn post_once(
    client: &reqwest::Client,
    url: &str,
    fields: &[(&str, &str)],
    json: bool,
) -> Result<Reply, String> {
    let request = client.post(url).header("accept", "application/json");
    let request = if json {
        let object: Map<String, Value> = fields
            .iter()
            .map(|(k, v)| ((*k).to_owned(), Value::String((*v).to_owned())))
            .collect();
        request
            .header("content-type", "application/json")
            .body(Value::Object(object).to_string())
    } else {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields)
            .finish();
        request
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
    };
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Ok(Reply { status, body })
}

/// [`post_once`] retried up to [`RETRIES`] times after a transport
/// failure or a 5xx answer.
///
/// # Errors
///
/// `auth.login_failed` when every attempt failed in transport.
pub async fn post(
    client: &reqwest::Client,
    url: &str,
    fields: &[(&str, &str)],
    json: bool,
) -> Result<Reply, Error> {
    let mut attempt = 0;
    loop {
        let outcome = post_once(client, url, fields, json).await;
        let retry = match &outcome {
            Ok(reply) => reply.status >= 500,
            Err(_) => true,
        };
        if !retry || attempt == RETRIES {
            return outcome.map_err(|why| {
                Error::new(
                    &codes::AUTH_LOGIN_FAILED,
                    format!("the sign-in request to {url} failed: {why}"),
                )
            });
        }
        attempt += 1;
        tokio::time::sleep(RETRY_PAUSE).await;
    }
}

/// Tokens from a token endpoint.
#[derive(Debug, Clone)]
pub struct TokenSet {
    /// The access token.
    pub access_token: SecretString,
    /// The refresh token, when one was issued.
    pub refresh_token: Option<SecretString>,
    /// When the access token expires.
    pub expires_at: Option<Timestamp>,
    /// The OpenID Connect id token, when one was issued.
    pub id_token: Option<SecretString>,
}

impl TokenSet {
    /// Reads a successful token response.
    ///
    /// # Errors
    ///
    /// `auth.login_failed` for an error status or a body without an
    /// `access_token`.
    pub fn from_reply(reply: &Reply, now: Timestamp) -> Result<Self, Error> {
        if !(200..300).contains(&reply.status) {
            return Err(failed(&format!(
                "the token endpoint refused the sign-in ({})",
                reply.describe()
            )));
        }
        let access = reply
            .field("access_token")
            .filter(|t| !t.is_empty())
            .ok_or_else(|| failed("the token endpoint returned no access_token"))?;
        let expires_at = reply
            .body
            .get("expires_in")
            .and_then(Value::as_i64)
            .and_then(|secs| now.checked_add(SignedDuration::from_secs(secs)).ok());
        let secret = |name: &str| {
            reply
                .field(name)
                .filter(|t| !t.is_empty())
                .map(|t| SecretString::new(t.to_owned()))
        };
        Ok(Self {
            access_token: SecretString::new(access.to_owned()),
            refresh_token: secret("refresh_token"),
            expires_at,
            id_token: secret("id_token"),
        })
    }
}

/// Exchanges an authorization `code` for tokens (PKCE `verifier`).
///
/// # Errors
///
/// `auth.login_failed`.
pub async fn exchange_code(
    client: &reqwest::Client,
    spec: &OAuthSpec,
    exchange: &Exchange<'_>,
) -> Result<TokenSet, Error> {
    let mut fields = vec![
        ("grant_type", "authorization_code"),
        ("code", exchange.code),
        ("redirect_uri", exchange.redirect_uri),
        ("client_id", spec.client_id.as_str()),
        ("code_verifier", exchange.verifier),
    ];
    let json = spec.token_encoding == TokenEncoding::JsonWithState;
    if let (true, Some(state)) = (json, exchange.state) {
        fields.push(("state", state));
    }
    let reply = post(client, &spec.token_url, &fields, json).await?;
    TokenSet::from_reply(&reply, Timestamp::now())
}

/// What a code exchange sends.
#[derive(Debug)]
pub struct Exchange<'a> {
    /// The authorization code.
    pub code: &'a str,
    /// The PKCE verifier.
    pub verifier: &'a str,
    /// The redirect URI of the authorization request.
    pub redirect_uri: &'a str,
    /// The authorization request's `state` (sent where the provider wants
    /// it back).
    pub state: Option<&'a str>,
}

/// Revokes `token` (a refresh token) at the spec's revocation endpoint
/// (RFC 7009); `Ok(false)` when the provider has none.
///
/// # Errors
///
/// `auth.login_failed` when the endpoint refuses or cannot be reached.
pub async fn revoke(
    client: &reqwest::Client,
    spec: &OAuthSpec,
    token: &SecretString,
) -> Result<bool, Error> {
    let Some(url) = &spec.revoke_url else {
        return Ok(false);
    };
    let fields = [
        ("token", token.expose()),
        ("token_type_hint", "refresh_token"),
        ("client_id", spec.client_id.as_str()),
    ];
    let json = spec.token_encoding == TokenEncoding::JsonWithState;
    let reply = post(client, url, &fields, json).await?;
    if (200..300).contains(&reply.status) {
        return Ok(true);
    }
    Err(failed(&format!(
        "the provider refused to revoke the refresh token ({})",
        reply.describe()
    )))
}

fn failed(message: &str) -> Error {
    Error::new(&codes::AUTH_LOGIN_FAILED, message)
}
