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
use crate::egress::{Denial, HostPolicy, Observer, Reason};
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
    hosts: Option<SessionHosts>,
}

/// The session egress policy a fetcher also obeys.
struct SessionHosts {
    hosts: Arc<HostPolicy>,
    requester: String,
    observer: Option<Observer>,
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
            hosts: None,
        })
    }

    /// Also refusing what the session's egress policy refuses, reporting
    /// each refusal of `requester` to `observer`.
    #[must_use]
    pub fn with_hosts(
        mut self,
        hosts: Arc<HostPolicy>,
        requester: &str,
        observer: Option<Observer>,
    ) -> Self {
        self.hosts = Some(SessionHosts {
            hosts,
            requester: requester.to_owned(),
            observer,
        });
        self
    }

    /// The same client and policies, with refusals reported as
    /// `requester`. Shares the connection pool and TLS setup: building a
    /// client loads the platform's trust roots, which costs milliseconds of
    /// session start on Linux.
    #[must_use]
    pub fn for_requester(&self, requester: &str) -> Self {
        Self {
            client: self.client.clone(),
            resolver: self.resolver.clone(),
            policy: Arc::clone(&self.policy),
            hosts: self.hosts.as_ref().map(|hosts| SessionHosts {
                hosts: Arc::clone(&hosts.hosts),
                requester: requester.to_owned(),
                observer: hosts.observer.clone(),
            }),
        }
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
                .map_err(|e| transport_error("GET", &url, &e))?;
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

    /// Sends one `method` request to `url` after the same checks as a
    /// [`Fetcher::get`] hop; `build` adds headers, body and timeout. A
    /// redirect comes back as the response: the caller decides whether its
    /// target is fetched (through these checks again).
    ///
    /// # Errors
    ///
    /// As [`Fetcher::get`], except `net.too_many_redirects`.
    pub async fn send(
        &self,
        method: reqwest::Method,
        url: &str,
        build: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    ) -> Result<Response, Error> {
        let url = parse_url(url)?;
        self.preflight(&url).await?;
        build(self.client.request(method.clone(), url.clone()))
            .send()
            .await
            .map_err(|e| transport_error(method.as_str(), &url, &e))
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
        self.check_session(host, port.unwrap_or_default())?;
        match self.check_destination(host, port).await {
            Ok(()) => Ok(()),
            Err(Blocked::Refused(refusal)) => {
                self.report(&refusal, port.unwrap_or_default());
                Err(refusal.into())
            }
            Err(Blocked::Failed(error)) => Err(error),
        }
    }

    /// The address checks of one hop.
    async fn check_destination(&self, host: &str, port: Option<u16>) -> Result<(), Blocked> {
        let literal = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = literal.parse::<IpAddr>() {
            return self
                .policy
                .check_addr(None, ip, port)
                .map_err(Blocked::Refused);
        }
        self.policy.check_host(host).map_err(Blocked::Refused)?;
        let addrs = self.resolver.resolve_checked(host).await.map_err(|e| {
            refusal_of(&e).map_or_else(
                || Blocked::Failed(Error::new(&codes::NET_REQUEST, e.to_string())),
                Blocked::Refused,
            )
        })?;
        for ip in addrs {
            self.policy
                .check_addr(Some(host), ip, port)
                .map_err(Blocked::Refused)?;
        }
        Ok(())
    }

    /// Tells the session's observer about an address refusal, as the
    /// egress proxy does (spec: security-boundaries, every denied
    /// connection produces an `egress.denied` event).
    fn report(&self, refusal: &Refusal, port: u16) {
        if let Some(SessionHosts {
            requester,
            observer: Some(observer),
            ..
        }) = &self.hosts
        {
            observer(Denial {
                host: refusal
                    .host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_owned(),
                port,
                requester: requester.clone(),
                reason: refusal.reason.into(),
            });
        }
    }
}

/// Why a hop may not be fetched.
enum Blocked {
    /// The destination policy refused it.
    Refused(Refusal),
    /// The check itself failed.
    Failed(Error),
}

impl Fetcher {
    /// The session egress check of one hop.
    fn check_session(&self, host: &str, port: u16) -> Result<(), Error> {
        let Some(session) = &self.hosts else {
            return Ok(());
        };
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        let Err(reason) = session.hosts.check(bare, port) else {
            return Ok(());
        };
        if let Some(observer) = &session.observer {
            observer(Denial {
                host: bare.to_owned(),
                port,
                requester: session.requester.clone(),
                reason,
            });
        }
        if reason == Reason::Denylist {
            return Err(Error::new(
                &codes::NET_DESTINATION_BLOCKED,
                format!("Blocked: {bare} is blocked by policy"),
            )
            .with_hint(
                "security.egress.deny, its deny_files and /egress deny list blocked hosts",
            ));
        }
        Err(Error::new(
            &codes::NET_DESTINATION_BLOCKED,
            format!(
                "destination {bare} is refused by the session's egress policy ({})",
                reason.as_str()
            ),
        )
        .with_hint("/egress status shows the policy; /egress allow <host> admits a host"))
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

fn transport_error(method: &str, url: &Url, error: &reqwest::Error) -> Error {
    if let Some(refusal) = refusal_of(error) {
        return refusal.into();
    }
    let mut message = format!("{method} {}: {error}", display(url));
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
