//! The browser sign-in: authorization code with PKCE (S256) and a random
//! `state`, received on the loopback listener (spec: model-providers,
//! Device-code and browser PKCE sign-in flows). In a remote session the
//! authorization URL comes with the `ssh -N -L` tunnel hint instead of a
//! browser.

use std::time::Duration;

use pmt_core::error::Error;
use tokio::time::Instant;
use url::Url;

use super::SignInUi;
use super::loopback::{Expect, Listener};
use super::pkce::{Pkce, random_token};
use super::spec::OAuthSpec;
use super::token::{self, Exchange, TokenSet};

/// How long the listener waits for the callback.
pub const CALLBACK_TIMEOUT: Duration = Duration::from_mins(5);

/// Where the sign-in runs.
#[derive(Debug, Clone, Copy)]
pub struct Place {
    /// An SSH session: print the tunnel hint, open no browser.
    pub remote: bool,
    /// A graphical browser may be opened.
    pub can_open_browser: bool,
    /// Callback wait.
    pub timeout: Duration,
}

/// The tunnel a remote user runs on their own machine.
#[must_use]
pub fn tunnel_hint(port: u16) -> String {
    format!(
        "This is a remote session: on the machine with your browser, forward the callback port first:\n  ssh -N -L {port}:127.0.0.1:{port} <this-host>"
    )
}

/// The listener for `spec`'s redirect. Bound first so a busy fixed port
/// is known before anything is printed.
///
/// # Errors
///
/// `auth.port_busy`, `auth.login_failed`.
pub async fn bind(spec: &OAuthSpec) -> Result<Listener, Error> {
    Listener::bind(spec.redirect.port).await
}

/// Runs the browser sign-in on `listener` to the end.
///
/// # Errors
///
/// `auth.login_failed` (refused, timed out, exchange failed).
pub async fn run(
    client: &reqwest::Client,
    spec: &OAuthSpec,
    listener: &Listener,
    ui: &dyn SignInUi,
    place: Place,
) -> Result<TokenSet, Error> {
    let pkce = Pkce::new()?;
    let state = random_token(32)?;
    let redirect_uri = format!(
        "http://{}:{}{}",
        spec.redirect.uri_host,
        listener.port(),
        spec.redirect.path
    );
    let url = authorize_url(spec, &redirect_uri, &pkce.challenge, state.expose())?;
    ui.say(&format!(
        "Open this URL to sign in to {}:\n  {url}",
        spec.provider
    ));
    if place.remote {
        ui.say(&tunnel_hint(listener.port()));
    } else if place.can_open_browser && ui.open_browser(&url) {
        ui.say("Opened the browser.");
    }
    ui.say(&format!(
        "Waiting for the callback on {redirect_uri} (timeout {} s)...",
        place.timeout.as_secs()
    ));
    let expect = Expect {
        path: &spec.redirect.path,
        state: state.expose(),
    };
    let code = listener
        .wait(&expect, Instant::now() + place.timeout)
        .await?;
    let exchange = Exchange {
        code: code.expose(),
        verifier: pkce.verifier.expose(),
        redirect_uri: &redirect_uri,
        state: Some(state.expose()),
    };
    token::exchange_code(client, spec, &exchange).await
}

/// The authorization request URL.
fn authorize_url(
    spec: &OAuthSpec,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> Result<String, Error> {
    let base = spec.authorize_url.as_deref().unwrap_or_default();
    let mut url = Url::parse(base).map_err(|e| {
        Error::new(
            &pmt_core::error::codes::AUTH_INSECURE_ENDPOINT,
            format!(
                "authorize URL '{base}' of provider '{}' is malformed: {e}",
                spec.provider
            ),
        )
    })?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", &spec.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", &spec.scopes.join(" "))
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state);
        for (key, value) in &spec.authorize_params {
            query.append_pair(key, value);
        }
    }
    Ok(url.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_hint_forwards_the_callback_port() {
        assert!(tunnel_hint(1455).contains("ssh -N -L 1455:127.0.0.1:1455"));
    }
}
