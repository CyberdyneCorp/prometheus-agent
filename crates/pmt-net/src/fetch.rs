//! SSRF-safe fetching of model- or remote-supplied URLs (spec:
//! security-boundaries, In-process fetch SSRF protection): web extraction,
//! media downloads, provider-returned media URLs, catalog and index
//! downloads, model-added MCP HTTP endpoints.
//!
//! Each hop is checked before any byte is sent: the host is resolved here,
//! every address must pass the [`DestinationPolicy`] (a failed resolution
//! is a refusal), and the client's resolver re-checks what it connects to.
//! Redirects are followed by hand, at most [`MAX_REDIRECTS`], each target
//! checked the same way.

use std::error::Error as _;
use std::fmt::Write as _;
use std::io;
use std::net::IpAddr;
use std::sync::Arc;

use pmt_core::error::{Error, codes};
use reqwest::{Response, Url, header, redirect};

use crate::destination::{DestinationPolicy, Refusal};
use crate::http::{OutboundPolicy, base_builder, build};
use crate::resolve::{Lookup, OutboundResolver, SystemLookup};

/// Redirect hops followed before the fetch fails.
pub const MAX_REDIRECTS: usize = 10;

/// A client for untrusted URLs. Build one per session (the policy carries
/// the session's grants).
pub struct Fetcher {
    client: reqwest::Client,
    resolver: OutboundResolver,
    policy: Arc<DestinationPolicy>,
}

impl Fetcher {
    /// A fetcher on the system resolver.
    ///
    /// # Errors
    ///
    /// As [`crate::client_builder`].
    pub fn new(outbound: &OutboundPolicy, destinations: DestinationPolicy) -> Result<Self, Error> {
        Self::with_lookup(outbound, destinations, Arc::new(SystemLookup))
    }

    /// [`Fetcher::new`] with a substitute resolver, for tests.
    ///
    /// # Errors
    ///
    /// As [`crate::client_builder`].
    pub fn with_lookup(
        outbound: &OutboundPolicy,
        destinations: DestinationPolicy,
        lookup: Arc<dyn Lookup>,
    ) -> Result<Self, Error> {
        let policy = Arc::new(destinations);
        let resolver = OutboundResolver {
            lookup,
            force_ipv4: outbound.force_ipv4,
            destinations: Some(Arc::clone(&policy)),
        };
        let client =
            build(base_builder(outbound, resolver.clone())?.redirect(redirect::Policy::none()))?;
        Ok(Self {
            client,
            resolver,
            policy,
        })
    }

    /// GETs `url`, following redirects through the destination checks.
    ///
    /// # Errors
    ///
    /// `net.invalid_url` (bad URL, non-http(s) scheme or `Location`),
    /// `net.destination_blocked` (private, metadata or unresolvable hop),
    /// `net.too_many_redirects`, `net.request` (transport failure).
    pub async fn get(&self, url: &str) -> Result<Response, Error> {
        let mut url = parse_url(url)?;
        for _ in 0..=MAX_REDIRECTS {
            self.preflight(&url).await?;
            let response = self
                .client
                .get(url.clone())
                .send()
                .await
                .map_err(|e| transport_error(&url, &e))?;
            match redirect_target(&url, &response)? {
                Some(next) => url = next,
                None => return Ok(response),
            }
        }
        Err(Error::new(
            &codes::NET_TOO_MANY_REDIRECTS,
            format!(
                "{} redirected more than {MAX_REDIRECTS} times",
                display(&url)
            ),
        ))
    }

    /// Runs the destination checks of the first hop of `url` without
    /// sending anything: for URLs handed to a third party (a web
    /// extraction backend) that fetches them itself.
    ///
    /// # Errors
    ///
    /// As [`Fetcher::get`], except the transport errors.
    pub async fn check(&self, url: &str) -> Result<(), Error> {
        self.preflight(&parse_url(url)?).await
    }

    /// Checks one hop: metadata names, IP literals, and every address the
    /// host resolves to (with the URL's port, for port-scoped grants).
    async fn preflight(&self, url: &Url) -> Result<(), Error> {
        let host = url.host_str().unwrap_or_default();
        let port = url.port_or_known_default();
        let literal = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = literal.parse::<IpAddr>() {
            return Ok(self.policy.check_addr(None, ip, port)?);
        }
        self.policy.check_host(host)?;
        let addrs = self.resolver.resolve_checked(host).await.map_err(|e| {
            refusal_of(&e).map_or_else(
                || Error::new(&codes::NET_REQUEST, e.to_string()),
                Error::from,
            )
        })?;
        for ip in addrs {
            self.policy.check_addr(Some(host), ip, port)?;
        }
        Ok(())
    }
}

fn parse_url(raw: &str) -> Result<Url, Error> {
    let invalid = |why: &str| {
        Error::new(
            &codes::NET_INVALID_URL,
            format!("cannot fetch '{raw}': {why}"),
        )
    };
    let url = Url::parse(raw).map_err(|e| invalid(&e.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid("only http:// and https:// URLs can be fetched"));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(invalid("the URL has no host"));
    }
    Ok(url)
}

/// The next hop of a redirect response, checked for shape (its
/// destination is checked by the next preflight).
fn redirect_target(from: &Url, response: &Response) -> Result<Option<Url>, Error> {
    if !response.status().is_redirection() {
        return Ok(None);
    }
    let Some(location) = response.headers().get(header::LOCATION) else {
        return Ok(None);
    };
    let location = location.to_str().map_err(|_| {
        Error::new(
            &codes::NET_INVALID_URL,
            format!("{} sent a non-text Location header", display(from)),
        )
    })?;
    let next = from.join(location).map_err(|e| {
        Error::new(
            &codes::NET_INVALID_URL,
            format!("{} redirected to an invalid URL: {e}", display(from)),
        )
    })?;
    parse_url(next.as_str()).map(Some)
}

/// The destination refusal inside a transport error chain, if any.
fn refusal_of(error: &(dyn std::error::Error + 'static)) -> Option<Refusal> {
    let mut current = Some(error);
    while let Some(e) = current {
        if let Some(refusal) = e.downcast_ref::<Refusal>() {
            return Some(refusal.clone());
        }
        if let Some(inner) = e.downcast_ref::<io::Error>().and_then(io::Error::get_ref)
            && let Some(refusal) = inner.downcast_ref::<Refusal>()
        {
            return Some(refusal.clone());
        }
        current = e.source();
    }
    None
}

fn transport_error(url: &Url, error: &reqwest::Error) -> Error {
    if let Some(refusal) = refusal_of(error) {
        return refusal.into();
    }
    let mut message = format!("GET {}: {error}", display(url));
    let mut source = error.source();
    while let Some(e) = source {
        let _ = write!(message, ": {e}");
        source = e.source();
    }
    Error::new(&codes::NET_REQUEST, message)
}

/// The URL without query and fragment (they may carry tokens).
fn display(url: &Url) -> String {
    let mut shown = url.clone();
    shown.set_query(None);
    shown.set_fragment(None);
    let _ = shown.set_password(None);
    let _ = shown.set_username("");
    shown.to_string()
}
