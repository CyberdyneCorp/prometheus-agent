//! `daemon.toml [a2a]` (spec: agent-interop, A2A server enablement and
//! binding): the daemon-wide A2A listener every profile with
//! `a2a.server.enabled` shares. A non-loopback address is refused unless
//! `public = true` AND every served profile has a peer token: an A2A
//! server without tokens answers only loopback callers.

use std::net::{IpAddr, SocketAddr};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The default A2A port.
pub const DEFAULT_PORT: u16 = 9900;

/// The refusal of a non-loopback bind without the opt-in and tokens.
pub const PUBLIC_REFUSED: &str =
    "non-loopback a2a.listen requires a2a.public = true and peer tokens";

/// `daemon.toml [a2a]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct A2aListener {
    /// Where the A2A server listens (`host:port`, an IPv6 literal bare or
    /// bracketed, or a bare address on port 9900).
    pub listen: String,
    /// Allow a non-loopback `listen` (every served profile then needs a
    /// peer token).
    pub public: bool,
}

impl Default for A2aListener {
    fn default() -> Self {
        Self {
            listen: format!("127.0.0.1:{DEFAULT_PORT}"),
            public: false,
        }
    }
}

impl A2aListener {
    /// The parsed listen address, whatever its exposure.
    ///
    /// # Errors
    ///
    /// The validation message of a malformed address.
    pub fn addr(&self) -> Result<SocketAddr, String> {
        let text = self.listen.trim();
        if let Ok(addr) = text.parse::<SocketAddr>() {
            return Ok(addr);
        }
        let bare = text.trim_start_matches('[').trim_end_matches(']');
        bare.parse::<IpAddr>()
            .map(|ip| SocketAddr::new(ip, DEFAULT_PORT))
            .map_err(|_| {
                format!(
                    "a2a.listen: '{}' is not an address such as 127.0.0.1:{DEFAULT_PORT}",
                    self.listen
                )
            })
    }

    /// Whether the address is loopback only.
    ///
    /// # Errors
    ///
    /// As [`A2aListener::addr`].
    pub fn loopback(&self) -> Result<bool, String> {
        self.addr().map(|addr| addr.ip().is_loopback())
    }

    /// The address checked against the served profiles: a non-loopback
    /// one needs `public = true` and `has_tokens` true for every served
    /// profile.
    ///
    /// # Errors
    ///
    /// [`PUBLIC_REFUSED`], or the message of a malformed address.
    pub fn checked(&self, mut has_tokens: impl FnMut() -> bool) -> Result<SocketAddr, String> {
        let addr = self.addr()?;
        if addr.ip().is_loopback() || (self.public && has_tokens()) {
            Ok(addr)
        } else {
            Err(PUBLIC_REFUSED.to_owned())
        }
    }

    /// Checks what `daemon.toml` alone can tell.
    ///
    /// # Errors
    ///
    /// As [`A2aListener::checked`] with tokens assumed present.
    pub fn validate(&self) -> Result<(), String> {
        self.checked(|| true).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::{A2aListener, PUBLIC_REFUSED};

    fn listener(listen: &str, public: bool) -> A2aListener {
        A2aListener {
            listen: listen.to_owned(),
            public,
        }
    }

    /// Scenario: Wildcard bind without tokens.
    #[test]
    fn a_wildcard_bind_without_tokens_fails() {
        for listen in ["0.0.0.0:9900", "::", "[::]:9900", "[::]", "192.168.1.4"] {
            assert_eq!(
                listener(listen, false).checked(|| true).unwrap_err(),
                PUBLIC_REFUSED,
                "{listen}"
            );
            assert_eq!(
                listener(listen, true).checked(|| false).unwrap_err(),
                PUBLIC_REFUSED,
                "{listen}"
            );
            assert!(listener(listen, true).checked(|| true).is_ok(), "{listen}");
        }
    }

    #[test]
    fn loopback_binds_need_nothing() {
        for listen in ["127.0.0.1:9900", "[::1]:9901", "::1", "127.0.0.1"] {
            assert!(
                listener(listen, false).checked(|| false).is_ok(),
                "{listen}"
            );
        }
        assert!(listener("nowhere", false).validate().is_err());
    }
}
