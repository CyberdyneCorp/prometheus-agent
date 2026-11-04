//! The egress policy of a profile and of each session (spec:
//! security-boundaries, Per-session network egress policy; Egress
//! enforcement for child processes).
//!
//! A session starts from `security.egress` (narrowed or extended by `chat
//! --egress/--allow-host`), keeps its [`HostPolicy`] for its lifetime
//! (`/egress`, `egress.set` change it in place) and runs its children
//! through one loopback [`Proxy`] that consults it. Every refusal becomes an
//! `egress.denied` trajectory event and a security audit record.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use pmt_config::ProfileScope;
use pmt_config::schema::{EgressConfig, EgressMode};
use pmt_core::audit::{AuditLog, Event, Kind};
use pmt_core::error::{Error, codes};
use pmt_net::DestinationPolicy;
use pmt_net::egress::{Allowed, Denial, HostPolicy, Mode, Observer};

/// The pmt-net mode of a configured mode.
#[must_use]
pub fn mode(mode: EgressMode) -> Mode {
    match mode {
        EgressMode::Public => Mode::Public,
        EgressMode::Allowlist => Mode::Allowlist,
        EgressMode::None => Mode::None,
    }
}

/// A session's starting overrides: `--egress` and `--allow-host`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    /// Replaces `security.egress.mode`.
    pub mode: Option<EgressMode>,
    /// Added to `security.egress.allow`.
    pub allow: Vec<String>,
}

/// The host policy `config` describes, with `overrides`.
#[must_use]
pub fn host_policy(config: &EgressConfig, overrides: &Overrides) -> HostPolicy {
    let allow = config
        .allow
        .iter()
        .chain(&overrides.allow)
        .map(|p| Allowed::parse(p))
        .collect();
    HostPolicy::new(mode(overrides.mode.unwrap_or(config.mode)), allow)
}

/// The address policy of `config`.
///
/// # Errors
///
/// `config.invalid` for a fake-IP range that is not CIDR notation.
pub fn destinations(config: &EgressConfig) -> Result<DestinationPolicy, Error> {
    DestinationPolicy::new(config.allow_private, &config.fake_ip_ranges)
}

/// Receives a refusal after the audit log (a session's trajectory).
pub type DenialSink = Arc<dyn Fn(&Denial) + Send + Sync>;

/// A running session's egress: its policy, its children's proxy (which
/// stops when this is dropped) and its entry in the process-wide table the
/// browser tools look a session's policy up in.
pub struct Running {
    /// The policy `/egress` and `egress.set` change.
    pub hosts: Arc<HostPolicy>,
    _proxy: Option<pmt_net::egress::Proxy>,
    _registration: Registration,
}

impl Running {
    /// The egress of `session` with the proxy that enforces it for the
    /// session's children.
    #[must_use]
    pub fn new(
        session: &str,
        egress: &SessionEgress,
        proxy: Option<pmt_net::egress::Proxy>,
    ) -> Self {
        Self {
            hosts: Arc::clone(&egress.hosts),
            _proxy: proxy,
            _registration: register(session, egress.clone()),
        }
    }
}

/// The running sessions' egress policies, by session id.
fn sessions() -> &'static Mutex<HashMap<String, SessionEgress>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, SessionEgress>>> = OnceLock::new();
    SESSIONS.get_or_init(Mutex::default)
}

/// Removes its session from the table when dropped.
struct Registration(String);

impl Drop for Registration {
    fn drop(&mut self) {
        sessions()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.0);
    }
}

fn register(session: &str, egress: SessionEgress) -> Registration {
    sessions()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(session.to_owned(), egress);
    Registration(session.to_owned())
}

/// The egress policy of running session `id` and where its refusals go
/// (the browser tools' lookup).
#[must_use]
pub fn session_policy(id: &str) -> Option<(Arc<HostPolicy>, Observer)> {
    sessions()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(id)
        .map(|e| (Arc::clone(&e.hosts), Arc::clone(&e.observer)))
}

/// An observer recording each refusal in the profile's audit log (and
/// passing it to `then`, the session's trajectory).
#[must_use]
pub fn observer(
    scope: &ProfileScope,
    session: Option<String>,
    then: Option<DenialSink>,
) -> Observer {
    let log = AuditLog::of(scope.dir());
    Arc::new(move |denial: Denial| {
        let mut event = Event::new(
            Kind::EgressDenied,
            format!("{}:{}", denial.host, denial.port),
            "denied",
        )
        .with_detail(format!("{} ({})", denial.requester, denial.reason.as_str()));
        if let Some(session) = &session {
            event = event.in_session(session.clone());
        }
        log.record(&event);
        if let Some(then) = &then {
            then(&denial);
        }
    })
}

/// Names the session a refusal belongs to, if any: the caller of a
/// shared child (an MCP stdio server, a python sidecar) or plugin call.
pub type Caller = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// An observer for connections made on behalf of whichever session
/// `caller` names when the refusal happens: a running session's refusal
/// goes to that session's own observer (audit record with its id and an
/// `egress.denied` event in its trajectory); otherwise it is recorded in
/// the profile's audit log, with the session id when there is one.
#[must_use]
pub fn attributed(scope: &ProfileScope, caller: Caller) -> Observer {
    let scope = scope.clone();
    Arc::new(move |denial: Denial| {
        let session = caller();
        if let Some((_, observer)) = session.as_deref().and_then(session_policy) {
            return observer(denial);
        }
        observer(&scope, session, None)(denial);
    })
}

/// The host policy a connection for `session` obeys: the running
/// session's own (with its `/egress` changes), else the profile's.
#[must_use]
pub fn hosts_for(scope: &ProfileScope, session: Option<&str>) -> Arc<HostPolicy> {
    session.and_then(session_policy).map_or_else(
        || {
            Arc::new(host_policy(
                &scope.config().security.egress,
                &Overrides::default(),
            ))
        },
        |(hosts, _)| hosts,
    )
}

/// A session's egress: its policy and where refusals go.
#[derive(Clone)]
pub struct SessionEgress {
    /// The session's mode and patterns (`/egress` changes them).
    pub hosts: Arc<HostPolicy>,
    /// Records each refusal.
    pub observer: Observer,
}

/// The loopback egress proxy of a sandboxed child whose `capabilities`
/// let it connect anywhere at all (`None` otherwise): it admits what
/// `hosts` and the `net.connect` capabilities both allow, a capability
/// naming a host also granting that host's private address (spec:
/// security-boundaries, Egress enforcement for child processes). The child
/// runs with [`pmt_sandbox::Policy::through_proxy`] and the proxy
/// variables.
///
/// # Errors
///
/// A bad `security.egress.fake_ip_ranges` entry; the proxy cannot bind.
pub fn child_proxy(
    scope: &ProfileScope,
    capabilities: &pmt_core::capability::CapabilitySet,
    hosts: Arc<HostPolicy>,
    requester: &str,
    observer: Observer,
) -> Result<Option<pmt_net::egress::Proxy>, Error> {
    let mut destinations = destinations(&scope.config().security.egress)?;
    let mut granted = Vec::new();
    let mut anywhere = false;
    for capability in capabilities {
        if let pmt_core::capability::Capability::NetConnect { host, port } = capability {
            anywhere |= host == "*";
            let pattern = Allowed {
                host: host.clone(),
                port: *port,
            };
            if let Ok(grant) = pmt_net::PrivateGrant::parse(&pattern.to_string()) {
                destinations = destinations.with_grant(grant);
            }
            granted.push(pattern);
        }
    }
    if granted.is_empty() {
        return Ok(None);
    }
    let mut rules = pmt_net::egress::Rules::new(hosts, destinations, requester).observed(observer);
    if !anywhere {
        rules = rules.granted(granted);
    }
    pmt_net::egress::Proxy::start(rules).map(Some).map_err(|e| {
        Error::new(
            &codes::HOME_IO,
            format!("the egress proxy of {requester} did not start: {e}"),
        )
    })
}

/// The policy of a child whose network goes through `proxy` (see
/// [`pmt_sandbox::Policy::through_proxy`]): on Linux the child's private
/// listener is served by `proxy`; on Windows the launcher runs a relay in
/// the child's `AppContainer` and connects it to `proxy` itself, so the
/// relay callback is unused there.
#[must_use]
pub fn through(policy: pmt_sandbox::Policy, proxy: &pmt_net::egress::Proxy) -> pmt_sandbox::Policy {
    #[cfg(unix)]
    let relay = {
        let attacher = proxy.attacher();
        pmt_sandbox::Relay::new(move |attached| attacher.serve(attached.listener, attached.alive))
    };
    #[cfg(not(unix))]
    let relay = pmt_sandbox::Relay::new(|_| {});
    policy.through_proxy(proxy.addr().port(), relay)
}

/// One `/egress` (or `egress.set`) change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Switch the mode.
    Mode(Mode),
    /// Admit a host pattern.
    Allow(String),
    /// Refuse a host pattern.
    Deny(String),
}

impl Change {
    /// Parses `mode <public|allowlist|none>`, `allow <pattern>` or `deny
    /// <pattern>`.
    ///
    /// # Errors
    ///
    /// `cli.usage` naming the accepted forms.
    pub fn parse(action: &str, value: &str) -> Result<Self, Error> {
        let value = value.trim();
        let usage = || {
            Error::new(
                &codes::CLI_USAGE,
                "usage: /egress [status] | allow <host[:port]> | deny <host[:port]> | mode <public|allowlist|none>",
            )
        };
        match action {
            "allow" if !value.is_empty() => Ok(Self::Allow(value.to_owned())),
            "deny" if !value.is_empty() => Ok(Self::Deny(value.to_owned())),
            "mode" => Mode::parse(value).map(Self::Mode).ok_or_else(usage),
            _ => Err(usage()),
        }
    }

    /// Applies the change.
    pub fn apply(&self, hosts: &HostPolicy) {
        match self {
            Self::Mode(mode) => hosts.set_mode(*mode),
            Self::Allow(pattern) => hosts.allow(Allowed::parse(pattern)),
            Self::Deny(pattern) => hosts.deny(Allowed::parse(pattern)),
        }
    }
}

/// `/egress status`: the mode and patterns in one block.
#[must_use]
pub fn describe(hosts: &HostPolicy) -> String {
    let state = hosts.state();
    let list = |patterns: &[Allowed]| {
        if patterns.is_empty() {
            "none".to_owned()
        } else {
            patterns
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    format!(
        "egress: {}\nallowed: {}\ndenied: {}",
        state.mode.as_str(),
        list(&state.allow),
        list(&state.deny)
    )
}

/// Applies an `egress.set` request: the mode, then each allowed and each
/// denied pattern.
///
/// # Errors
///
/// `cli.usage` for an unknown mode (nothing is changed).
pub fn apply_set(
    hosts: &HostPolicy,
    mode: Option<&str>,
    allow: &[String],
    deny: &[String],
) -> Result<(), Error> {
    let mut changes = Vec::new();
    if let Some(mode) = mode {
        changes.push(Change::parse("mode", mode)?);
    }
    for pattern in allow {
        changes.push(Change::parse("allow", pattern)?);
    }
    for pattern in deny {
        changes.push(Change::parse("deny", pattern)?);
    }
    for change in &changes {
        change.apply(hosts);
    }
    Ok(())
}

/// The policy as `(mode, allow, deny)` strings.
#[must_use]
pub fn snapshot(hosts: &HostPolicy) -> (String, Vec<String>, Vec<String>) {
    let state = hosts.state();
    let texts = |patterns: &[Allowed]| patterns.iter().map(ToString::to_string).collect();
    (
        state.mode.as_str().to_owned(),
        texts(&state.allow),
        texts(&state.deny),
    )
}

/// Sends a chat's `--egress`/`--allow-host` to a daemon session (`egress.set`);
/// nothing when neither was given.
///
/// # Errors
///
/// The daemon's refusal.
pub async fn send(
    client: &pmt_rpc::client::Client,
    session: &str,
    overrides: &Overrides,
) -> Result<(), Error> {
    if overrides.mode.is_none() && overrides.allow.is_empty() {
        return Ok(());
    }
    let params = pmt_rpc::contracts::EgressSetParams {
        session_id: session.to_owned(),
        mode: overrides.mode.map(|m| m.as_str().to_owned()),
        allow: overrides.allow.clone(),
        deny: Vec::new(),
    };
    client
        .call_typed::<pmt_rpc::contracts::EgressSet>(&params)
        .await
        .map(|_| ())
        .map_err(|e| pmt_rpc::client::error_of(&e))
}

#[cfg(test)]
mod tests {
    //! Every denied connection produces an `egress.denied` event of the
    //! session it was made for (spec: security-boundaries, Per-session
    //! network egress policy), including connections of children shared
    //! by sessions (MCP stdio servers, python sidecars).

    use pmt_config::{Home, Launch, Overrides as ScopeOverrides, ProfileName};
    use pmt_net::egress::Reason;

    use super::*;

    fn scope(root: &std::path::Path) -> ProfileScope {
        let home = Home::open(root).unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        ProfileScope::open(profile, &ScopeOverrides::default(), Launch::Other).unwrap()
    }

    fn denial(host: &str) -> Denial {
        Denial {
            host: host.to_owned(),
            port: 443,
            requester: "mcp:files".to_owned(),
            reason: Reason::NotAllowlisted,
        }
    }

    #[test]
    fn a_shared_childs_refusal_goes_to_the_calling_session() {
        let root = tempfile::tempdir().unwrap();
        let scope = scope(root.path());
        let seen: Arc<Mutex<Vec<Denial>>> = Arc::default();
        let sink = Arc::clone(&seen);
        let session = SessionEgress {
            hosts: Arc::new(HostPolicy::new(Mode::Allowlist, Vec::new())),
            observer: Arc::new(move |d| sink.lock().unwrap().push(d)),
        };
        let _running = Running::new("ses_calling", &session, None);
        let current: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(Some("ses_calling".into())));
        let now = Arc::clone(&current);
        let observer = attributed(&scope, Arc::new(move || now.lock().unwrap().clone()));
        observer(denial("exfil.example"));
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "the calling session's observer"
        );
        *current.lock().unwrap() = None;
        observer(denial("idle.example"));
        assert_eq!(seen.lock().unwrap().len(), 1);
        let audit = std::fs::read_to_string(scope.dir().join(pmt_core::audit::FILE)).unwrap();
        assert!(audit.contains("idle.example:443"), "{audit}");
        assert!(audit.contains("mcp:files (not_allowlisted)"), "{audit}");
    }
}
