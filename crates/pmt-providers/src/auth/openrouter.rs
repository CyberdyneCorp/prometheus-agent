//! OpenRouter OAuth key minting (spec: model-providers, OpenRouter OAuth
//! key minting).
//!
//! `prometheus auth add openrouter --type oauth` runs browser PKCE against
//! OpenRouter. OpenRouter echoes no `state`, so the loopback callback path
//! carries a random nonce (`/cb/<nonce>`) and every other path is answered
//! 404 without exchanging anything. The code is exchanged for a user API
//! key, which joins the provider's credential pool as an `api_key`
//! credential whose source is `oauth`; with no refresh token behind it, a
//! later 401 marks it `dead` (`auth.reauth_required`).

use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use serde_json::Value;
use tokio::time::Instant;

use super::SignInUi;
use super::browser::{Place, tunnel_hint};
use super::loopback::{Expect, Listener};
use super::pkce::{Pkce, random_token};
use super::spec::{self, OAuthSpec};
use crate::registry::Provider;

/// The credential source recorded for a key minted this way.
pub const SOURCE: &str = "oauth";

/// Runs the browser sign-in of `provider` (OpenRouter's endpoints, or
/// `[providers.<id>.oauth]`) and returns the minted API key.
///
/// # Errors
///
/// `auth.insecure_endpoint`, `auth.host_not_allowed`, `auth.port_busy`,
/// `auth.login_failed` (refused, timed out, exchange failed).
pub async fn mint_key(
    client: &reqwest::Client,
    provider: &Provider,
    ui: &dyn SignInUi,
    place: Place,
) -> Result<SecretString, Error> {
    let spec = spec::for_provider(provider)?;
    spec.check()?;
    let listener = Listener::bind(spec.redirect.port).await?;
    let nonce = random_token(24)?;
    let path = format!(
        "{}/{}",
        spec.redirect.path.trim_end_matches('/'),
        nonce.expose()
    );
    let callback = format!(
        "http://{}:{}{path}",
        spec.redirect.uri_host,
        listener.port()
    );
    let pkce = Pkce::new()?;
    let url = authorize_url(&spec, &callback, &pkce.challenge)?;
    ui.say(&format!(
        "Open this URL to create an OpenRouter key for Prometheus:\n  {url}"
    ));
    if place.remote {
        ui.say(&tunnel_hint(listener.port()));
    } else if place.can_open_browser && ui.open_browser(&url) {
        ui.say("Opened the browser.");
    }
    let expect = Expect {
        path: &path,
        state: None,
    };
    let code = listener
        .wait(&expect, Instant::now() + place.timeout)
        .await?;
    exchange(client, &spec, &code, &pkce.verifier).await
}

fn authorize_url(spec: &OAuthSpec, callback: &str, challenge: &str) -> Result<String, Error> {
    let base = spec.authorize_url.as_deref().ok_or_else(|| {
        Error::new(
            &codes::AUTH_SIGNIN_UNAVAILABLE,
            format!("provider '{}' has no authorize URL", spec.provider),
        )
    })?;
    let mut url = url::Url::parse(base).map_err(|e| {
        Error::new(
            &codes::AUTH_INSECURE_ENDPOINT,
            format!("authorize URL '{base}': {e}"),
        )
    })?;
    url.query_pairs_mut()
        .append_pair("callback_url", callback)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url.into())
}

/// Exchanges `code` (with the PKCE `verifier`) for the user's API key.
async fn exchange(
    client: &reqwest::Client,
    spec: &OAuthSpec,
    code: &SecretString,
    verifier: &SecretString,
) -> Result<SecretString, Error> {
    let fields = [
        ("code", code.expose()),
        ("code_verifier", verifier.expose()),
        ("code_challenge_method", "S256"),
    ];
    let reply = super::token::post(client, &spec.token_url, &fields, true).await?;
    let failed = |why: &str| {
        Error::new(
            &codes::AUTH_LOGIN_FAILED,
            format!("OpenRouter did not issue a key: {why}"),
        )
    };
    if reply.status != 200 {
        return Err(failed(&format!("HTTP {}", reply.status)));
    }
    match &reply.body["key"] {
        Value::String(key) if !key.is_empty() => Ok(SecretString::new(key.clone())),
        _ => Err(failed("the answer holds no key")),
    }
}
