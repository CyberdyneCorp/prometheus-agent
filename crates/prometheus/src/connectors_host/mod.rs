//! Hosted connectors of a profile (spec: connectors): the gateway client
//! over the profile's Nous sign-in, the session's connector tools behind
//! the tool-search bridge and its `manage_connections`.
//!
//! The sign-in is only ever read here: without one the profile has no
//! connectors, and nothing creates an identity.

#[cfg(feature = "mcp")]
mod mcp;

use std::sync::Arc;
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_net::OutboundPolicy;
use pmt_providers::resolve::CredentialExtras;
use pmt_tools::RegisteredTool;
use pmt_tools::connectors::manage::{self, Manager, Unavailable};
use pmt_tools::connectors::{Connectors, Gateway, TokenSource};
use pmt_tools::probe::{FnProbe, ProbeContext};

/// The provider whose sign-in reaches the connector gateway.
const NOUS: &str = "nous";

/// Reads the profile's Nous access token on every request.
fn token_source(scope: &ProfileScope) -> TokenSource {
    let credentials = pmt_providers::auth::Credentials::new(scope.secrets().store().clone());
    Arc::new(move || {
        CredentialExtras::subscription(&credentials, NOUS)
            .ok()
            .flatten()
            .map(|signed_in| signed_in.token.expose().to_owned())
    })
}

/// The gateway of `scope`, or why connectors are not offered.
///
/// # Errors
///
/// [`Unavailable`]: turned off, or no Nous sign-in.
pub fn hosted(scope: &ProfileScope) -> Result<Gateway, Unavailable> {
    let config = scope.config();
    if !config.connectors.enabled {
        return Err(Unavailable::Disabled);
    }
    let token = token_source(scope);
    if token().is_none() {
        return Err(Unavailable::SignedOut);
    }
    let url = &config.connectors.gateway_url;
    if let Err(why) = check_origin(url) {
        tracing::warn!(url, "connector gateway ignored: {why}");
        return Err(Unavailable::Disabled);
    }
    let policy = OutboundPolicy {
        force_ipv4: config.network.force_ipv4,
        extra_ca_file: config.network.extra_ca_file.as_ref().map(Into::into),
        proxy: config.network.proxy.clone(),
    };
    let http = pmt_net::client(&policy).map_err(|err| {
        tracing::warn!("connector gateway client: {}", err.message());
        Unavailable::Disabled
    })?;
    Ok(Gateway::new(url, http, token))
}

/// The gateway, or the CLI error that explains why there is none.
///
/// # Errors
///
/// `connectors.disabled`, `connectors.signin_required` (exit 78).
pub fn gateway_or_error(scope: &ProfileScope) -> Result<Gateway, Error> {
    hosted(scope).map_err(|why| match why {
        Unavailable::Disabled => Error::new(
            &codes::CONNECTORS_DISABLED,
            "connectors are turned off in this profile",
        )
        .with_hint("set connectors.enabled = true (and connectors.gateway_url to an https URL)"),
        Unavailable::SignedOut => Error::new(
            &codes::CONNECTORS_SIGNIN_REQUIRED,
            "connectors need a Nous sign-in: run prometheus auth login nous",
        ),
    })
}

/// The bearer token travels only over TLS, except to a loopback test or
/// self-hosted gateway.
fn check_origin(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("not a URL: {e}"))?;
    let loopback = match parsed.host() {
        Some(url::Host::Domain(host)) => host == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if loopback => Ok(()),
        scheme => Err(format!(
            "{scheme} is not allowed (https, or http on loopback)"
        )),
    }
}

/// The connector tools of a session of `scope` (`None`: not offered).
#[must_use]
pub fn remote(scope: &ProfileScope) -> Option<Arc<Connectors>> {
    hosted(scope).ok().map(Connectors::new)
}

/// The session's `manage_connections`; `cards`: its surface shows
/// connection cards.
#[must_use]
pub fn manager(scope: &ProfileScope, cards: bool) -> Arc<Manager> {
    let timeout = Duration::from_secs(scope.config().connectors.connect_timeout_secs);
    let manager = Manager::new(hosted(scope), timeout);
    let manager = if cards { manager.with_cards() } else { manager };
    #[cfg(feature = "mcp")]
    let manager = manager.with_mcp(Arc::new(mcp::CatalogTargets::new(scope.clone(), cards)));
    Arc::new(manager)
}

/// `manage_connections`' registry entry: available when connectors are
/// on or the catalog offers MCP servers.
#[must_use]
pub fn registered_tool() -> RegisteredTool {
    let catalog_has_mcp = !pmt_plugins::catalog::Catalog::merged(None).mcp.is_empty();
    manage::registered().with_probe(Arc::new(FnProbe::new(
        "connections",
        move |context: ProbeContext<'_>| {
            if context.config.connectors.enabled || catalog_has_mcp {
                Ok(())
            } else {
                Err("connectors_disabled".to_owned())
            }
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::check_origin;

    #[test]
    fn the_token_goes_only_over_tls_or_to_loopback() {
        assert!(check_origin("https://connector-gateway.nousresearch.com").is_ok());
        assert!(check_origin("http://127.0.0.1:8080").is_ok());
        assert!(check_origin("http://localhost:1").is_ok());
        assert!(check_origin("http://gateway.example").is_err());
        assert!(check_origin("ftp://x").is_err());
    }
}
