//! Which hosts a session's outbound connections may reach (spec:
//! security-boundaries, Per-session network egress policy).
//!
//! A [`HostPolicy`] holds the session's egress mode and its allow and deny
//! patterns. It is shared: `/egress allow|deny` and `egress.set` change it
//! while the session's proxy and in-process fetches keep consulting it, so
//! a change applies to the next connection without touching the system
//! prompt or the tool array.

use std::fmt;
use std::sync::{PoisonError, RwLock};

/// One host pattern: a host (`*.suffix` for subdomains, `*` for every
/// host) and an optional port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allowed {
    /// `api.example.com`, `*.example.com` or `*`.
    pub host: String,
    /// `None`: every port.
    pub port: Option<u16>,
}

impl Allowed {
    /// Parses `host[:port]` (an IPv6 literal in brackets).
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let text = text.trim();
        let (host, port) = match text.strip_prefix('[') {
            Some(rest) => match rest.split_once(']') {
                Some((host, tail)) => (host, tail.strip_prefix(':').and_then(|p| p.parse().ok())),
                None => (rest, None),
            },
            None => match text.rsplit_once(':') {
                Some((host, port)) if !host.contains(':') && port.parse::<u16>().is_ok() => {
                    (host, port.parse().ok())
                }
                _ => (text, None),
            },
        };
        Self {
            host: host.trim_end_matches('.').to_ascii_lowercase(),
            port,
        }
    }

    /// Whether this pattern names `host:port` (`host` lower case, no
    /// trailing dot).
    #[must_use]
    pub fn admits(&self, host: &str, port: u16) -> bool {
        let host_ok = match self.host.strip_prefix("*.") {
            _ if self.host == "*" => true,
            Some(suffix) => host.ends_with(&format!(".{suffix}")),
            None => self.host == host,
        };
        host_ok && self.port.is_none_or(|p| p == port)
    }
}

impl fmt::Display for Allowed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        match self.port {
            Some(port) => write!(f, "{host}:{port}"),
            None => f.write_str(&host),
        }
    }
}

/// The egress mode (`security.egress.mode`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// Any public address; private and metadata ranges denied.
    #[default]
    Public,
    /// Only hosts an allow pattern names.
    Allowlist,
    /// Nothing.
    None,
}

impl Mode {
    /// The configuration spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Allowlist => "allowlist",
            Self::None => "none",
        }
    }

    /// Parses a configuration spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        [Self::Public, Self::Allowlist, Self::None]
            .into_iter()
            .find(|m| m.as_str() == text)
    }
}

/// Why a connection was refused (the `reason` of `egress.denied`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The session's mode is `none`.
    ModeNone,
    /// `allowlist` mode and no allow pattern names the host.
    NotAllowlisted,
    /// A session deny pattern names the host.
    DeniedBySession,
    /// The requester's own grants do not name the host.
    NotGranted,
    /// A cloud metadata endpoint.
    Metadata,
    /// A private address without `allow_private` or a grant.
    PrivateRange,
    /// The name did not resolve.
    Unresolved,
    /// The checked address could not be reached.
    Unreachable,
}

impl Reason {
    /// The trajectory spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModeNone => "mode_none",
            Self::NotAllowlisted => "not_allowlisted",
            Self::DeniedBySession => "denied_by_session",
            Self::NotGranted => "not_granted",
            Self::Metadata => "metadata",
            Self::PrivateRange => "private_range",
            Self::Unresolved => "unresolved",
            Self::Unreachable => "unreachable",
        }
    }

    /// Whether the policy refused the connection (as opposed to the
    /// network failing it).
    #[must_use]
    pub const fn is_policy(self) -> bool {
        !matches!(self, Self::Unreachable)
    }
}

impl From<crate::destination::RefusalReason> for Reason {
    fn from(reason: crate::destination::RefusalReason) -> Self {
        use crate::destination::RefusalReason;
        match reason {
            RefusalReason::Metadata => Self::Metadata,
            RefusalReason::Private => Self::PrivateRange,
            RefusalReason::Unresolved => Self::Unresolved,
        }
    }
}

/// A refused connection: what `egress.denied` records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    /// The destination host as asked.
    pub host: String,
    /// The destination port.
    pub port: u16,
    /// Who asked (`terminal`, `plugin:<id>`, `browser`...).
    pub requester: String,
    /// Why.
    pub reason: Reason,
}

/// A snapshot of a [`HostPolicy`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    /// The mode.
    pub mode: Mode,
    /// Allow patterns (`allowlist` mode admits only these).
    pub allow: Vec<Allowed>,
    /// Deny patterns (refused in every mode).
    pub deny: Vec<Allowed>,
}

/// A session's egress mode and patterns, shared and changeable.
#[derive(Debug, Default)]
pub struct HostPolicy {
    state: RwLock<State>,
}

impl HostPolicy {
    /// A policy in `mode` admitting `allow` (in `allowlist` mode).
    #[must_use]
    pub fn new(mode: Mode, allow: Vec<Allowed>) -> Self {
        Self {
            state: RwLock::new(State {
                mode,
                allow,
                deny: Vec::new(),
            }),
        }
    }

    /// Checks `host:port` (case and a trailing dot are ignored).
    ///
    /// # Errors
    ///
    /// Why the host may not be reached.
    pub fn check(&self, host: &str, port: u16) -> Result<(), Reason> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let state = self.state();
        if state.mode == Mode::None {
            return Err(Reason::ModeNone);
        }
        if state.deny.iter().any(|d| d.admits(&host, port)) {
            return Err(Reason::DeniedBySession);
        }
        if state.mode == Mode::Allowlist && !state.allow.iter().any(|a| a.admits(&host, port)) {
            return Err(Reason::NotAllowlisted);
        }
        Ok(())
    }

    /// The policy now.
    #[must_use]
    pub fn state(&self) -> State {
        self.state
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn change(&self, edit: impl FnOnce(&mut State)) {
        edit(&mut self.state.write().unwrap_or_else(PoisonError::into_inner));
    }

    /// Switches the mode.
    pub fn set_mode(&self, mode: Mode) {
        self.change(|s| s.mode = mode);
    }

    /// Admits `pattern` (and lifts an identical deny pattern).
    pub fn allow(&self, pattern: Allowed) {
        self.change(|s| {
            s.deny.retain(|d| *d != pattern);
            if !s.allow.contains(&pattern) {
                s.allow.push(pattern);
            }
        });
    }

    /// Refuses `pattern` in every mode (and drops an identical allow
    /// pattern).
    pub fn deny(&self, pattern: Allowed) {
        self.change(|s| {
            s.allow.retain(|a| *a != pattern);
            if !s.deny.contains(&pattern) {
                s.deny.push(pattern);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_admit_what_they_say() {
        let public = HostPolicy::new(Mode::Public, Vec::new());
        assert_eq!(public.check("example.com", 443), Ok(()));
        let none = HostPolicy::new(Mode::None, vec![Allowed::parse("example.com")]);
        assert_eq!(none.check("example.com", 443), Err(Reason::ModeNone));
        let list = HostPolicy::new(Mode::Allowlist, vec![Allowed::parse("*.github.com:443")]);
        assert_eq!(list.check("API.github.com.", 443), Ok(()));
        assert_eq!(list.check("github.com", 443), Err(Reason::NotAllowlisted));
        assert_eq!(
            list.check("api.github.com", 80),
            Err(Reason::NotAllowlisted)
        );
    }

    #[test]
    fn session_changes_apply_to_the_next_check() {
        let policy = HostPolicy::new(Mode::Allowlist, Vec::new());
        assert!(policy.check("pypi.org", 443).is_err());
        policy.allow(Allowed::parse("pypi.org"));
        assert_eq!(policy.check("pypi.org", 443), Ok(()));
        policy.deny(Allowed::parse("pypi.org"));
        assert_eq!(policy.check("pypi.org", 443), Err(Reason::DeniedBySession));
        policy.set_mode(Mode::Public);
        assert_eq!(policy.check("pypi.org", 443), Err(Reason::DeniedBySession));
        assert_eq!(policy.check("example.com", 443), Ok(()));
    }

    #[test]
    fn patterns_parse_ports_and_ipv6() {
        assert_eq!(Allowed::parse("[::1]:8080").to_string(), "[::1]:8080");
        assert_eq!(Allowed::parse("Example.COM").to_string(), "example.com");
        assert!(Allowed::parse("*").admits("anything.example", 1));
    }
}
