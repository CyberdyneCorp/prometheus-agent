//! External surfaces fail closed (spec: security-boundaries, External
//! surfaces fail closed): listeners bind loopback unless their address is
//! set explicitly AND an allowlist or auth provider is configured, and a
//! surface without either dispatches nothing.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Mutex;

use pmt_core::error::{Error, codes};
use tokio::net::TcpListener;

/// The dashboard's default bind address.
pub const DASHBOARD_DEFAULT: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9120);

/// What a surface's configuration says about exposing it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Exposure {
    /// The bind address was set explicitly in configuration (not a
    /// default, not inferred).
    pub explicit_address: bool,
    /// An allowlist or auth provider is configured for the surface.
    pub authenticated: bool,
}

/// Whether `ip` only reaches the local host.
#[must_use]
pub fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// Refuses a non-loopback `addr` for `surface` unless `exposure` allows it.
///
/// # Errors
///
/// `net.listen_refused` naming the surface, the address and what is
/// missing.
pub fn check_bind(surface: &str, addr: SocketAddr, exposure: Exposure) -> Result<(), Error> {
    if is_loopback(addr.ip()) {
        return Ok(());
    }
    let missing = if !exposure.explicit_address {
        "a non-loopback address needs an explicit bind address in configuration"
    } else if !exposure.authenticated {
        "a non-loopback address needs an allowlist or auth provider"
    } else {
        return Ok(());
    };
    Err(Error::new(
        &codes::NET_LISTEN_REFUSED,
        format!("{surface}: refusing to listen on {addr}: {missing}"),
    )
    .with_hint("bind 127.0.0.1, or configure the address and an allowlist/auth provider"))
}

/// Binds a TCP listener for `surface` after [`check_bind`].
///
/// # Errors
///
/// `net.listen_refused`, or `net.listen` when the bind fails.
pub async fn bind(
    surface: &str,
    addr: SocketAddr,
    exposure: Exposure,
) -> Result<TcpListener, Error> {
    check_bind(surface, addr, exposure)?;
    TcpListener::bind(addr).await.map_err(|e| {
        Error::new(
            &codes::NET_LISTEN,
            format!("{surface}: cannot listen on {addr}: {e}"),
        )
        .with_source(e)
    })
}

/// The outcome of [`SurfaceGate::admit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Dispatch the sender's work.
    Admitted,
    /// Drop it; `first` is true the first time this sender was refused
    /// (the refusal is logged then, once per sender).
    Refused {
        /// First refusal of this sender.
        first: bool,
    },
}

/// The dispatch gate of a network-exposed surface (messaging adapter,
/// webhook, API): nothing is dispatched, no approval resolved and no output
/// relayed until an allowlist or an auth provider is configured.
#[derive(Debug)]
pub struct SurfaceGate {
    surface: String,
    allowlist: Option<HashSet<String>>,
    auth_provider: bool,
    refused: Mutex<HashSet<String>>,
}

impl SurfaceGate {
    /// A gate for `surface` with its configured allowlist (`None` = not
    /// configured) and whether an auth provider is configured.
    #[must_use]
    pub fn new(surface: &str, allowlist: Option<Vec<String>>, auth_provider: bool) -> Self {
        Self {
            surface: surface.to_owned(),
            allowlist: allowlist.map(|list| list.into_iter().collect()),
            auth_provider,
            refused: Mutex::new(HashSet::new()),
        }
    }

    /// Whether the surface is configured to accept anyone at all.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.allowlist.is_some() || self.auth_provider
    }

    /// Decides whether `sender`'s message may be dispatched. An auth
    /// provider is trusted to have authenticated `sender` already.
    pub fn admit(&self, sender: &str) -> Admission {
        let allowed = match &self.allowlist {
            Some(list) => list.contains(sender),
            None => self.auth_provider,
        };
        if allowed {
            return Admission::Admitted;
        }
        let first = self
            .refused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(sender.to_owned());
        if first {
            let why = if self.is_open() {
                "sender is not in the allowlist"
            } else {
                "no allowlist or auth provider is configured"
            };
            tracing::warn!(surface = %self.surface, sender, "refused message: {why}");
        }
        Admission::Refused { first }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::{Admission, DASHBOARD_DEFAULT, Exposure, SurfaceGate, bind, check_bind};

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[tokio::test]
    async fn non_loopback_bind_without_explicit_config_is_refused() {
        let err = bind("dashboard", addr("0.0.0.0:0"), Exposure::default())
            .await
            .unwrap_err();
        assert_eq!(err.code().code, "net.listen_refused");
        assert_eq!(err.exit_code(), 77);
        assert!(err.message().contains("explicit bind address"), "{err}");

        let explicit_only = Exposure {
            explicit_address: true,
            authenticated: false,
        };
        let err = bind("api", addr("[::]:0"), explicit_only)
            .await
            .unwrap_err();
        assert!(
            err.message().contains("allowlist or auth provider"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn loopback_binds_and_explicit_authenticated_exposure_is_allowed() {
        let listener = bind("dashboard", addr("127.0.0.1:0"), Exposure::default())
            .await
            .unwrap();
        assert!(listener.local_addr().unwrap().ip().is_loopback());
        assert!(check_bind("x", addr("[::ffff:127.0.0.1]:1"), Exposure::default()).is_ok());
        let exposed = Exposure {
            explicit_address: true,
            authenticated: true,
        };
        assert!(check_bind("api", addr("0.0.0.0:8080"), exposed).is_ok());
        assert_eq!(DASHBOARD_DEFAULT, addr("127.0.0.1:9120"));
    }

    #[test]
    fn adapter_without_allowlist_dispatches_nothing_and_logs_once_per_sender() {
        let gate = SurfaceGate::new("telegram", None, false);
        assert_eq!(gate.admit("alice"), Admission::Refused { first: true });
        assert_eq!(gate.admit("alice"), Admission::Refused { first: false });
        assert_eq!(gate.admit("bob"), Admission::Refused { first: true });

        let gate = SurfaceGate::new("telegram", Some(vec!["alice".into()]), false);
        assert_eq!(gate.admit("alice"), Admission::Admitted);
        assert_eq!(gate.admit("mallory"), Admission::Refused { first: true });
        assert_eq!(
            SurfaceGate::new("api", None, true).admit("token-user"),
            Admission::Admitted
        );
    }
}
