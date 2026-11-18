//! Where the proxy's container listener binds (spec: security-boundaries,
//! Egress enforcement for child processes: "loopback, or the container
//! bridge address, never `0.0.0.0`").
//!
//! Containers reach the host differently per runtime, and the listener
//! goes where they arrive:
//!
//! - Docker Desktop and Podman machines (every non-Linux host, and Docker
//!   Desktop on Linux) forward the host alias to the host's loopback.
//! - A rootful runtime on Linux routes its bridge gateway to the host.
//!   Podman creates its bridge with the first container on it, so the
//!   listener binds the gateway before the address exists (Linux
//!   `IP_FREEBIND`).
//! - A rootless runtime keeps its bridge inside its own network namespace
//!   (the gateway is no host address) and reaches the host at the host's
//!   own address through slirp4netns or pasta. That address also faces the
//!   network, so the listener there serves only connections coming from
//!   the host itself, and the relay maps the host alias to it explicitly.

use std::io;
use std::net::{IpAddr, SocketAddr, TcpListener};

use pmt_config::ProfileScope;

/// Where the container listener binds and who may use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// The host's loopback (forwarded by the runtime's VM, or no runtime).
    Loopback,
    /// A rootful runtime's bridge gateway.
    Gateway(IpAddr),
    /// A rootless runtime: the host's own address.
    Host(IpAddr),
}

const LOOPBACK: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

impl Reach {
    /// The reach of `scope`'s container runtime
    /// (`terminal.container.runtime`).
    #[must_use]
    pub fn of(scope: &ProfileScope) -> Self {
        if !cfg!(target_os = "linux") {
            return Self::Loopback;
        }
        let setting = &scope.config().terminal.container.runtime;
        let Ok(runtime) = pmt_sandbox::container::Runtime::detect(setting) else {
            return Self::Loopback;
        };
        let any = pmt_config::schema::SandboxConfig {
            allow_degraded: vec![pmt_config::schema::DegradedFeature::All],
            ..pmt_config::schema::SandboxConfig::default()
        };
        match runtime.probe(&any) {
            Ok(probe) if probe.vm => Self::Loopback,
            Ok(probe) if probe.rootless => host_address().map_or(Self::Loopback, Self::Host),
            Ok(_) => gateway(&runtime).map_or(Self::Loopback, Self::Gateway),
            Err(_) => Self::Loopback,
        }
    }

    /// The listener's address on `port`.
    #[must_use]
    pub fn addr(self, port: u16) -> SocketAddr {
        let ip = match self {
            Self::Loopback => LOOPBACK,
            Self::Gateway(ip) | Self::Host(ip) => ip,
        };
        SocketAddr::new(ip, port)
    }

    /// The only peer address the listener serves (`None`: any peer).
    #[must_use]
    pub fn only_from(self) -> Option<IpAddr> {
        match self {
            Self::Host(ip) => Some(ip),
            Self::Loopback | Self::Gateway(_) => None,
        }
    }

    /// The address the relay maps the host alias to (`None`: the
    /// runtime's own mapping).
    #[must_use]
    pub fn relay_host(self) -> Option<IpAddr> {
        self.only_from()
    }

    /// Binds the listener on `port`.
    ///
    /// # Errors
    ///
    /// The bind's error.
    pub fn bind(self, port: u16) -> io::Result<TcpListener> {
        let addr = self.addr(port);
        match self {
            Self::Gateway(_) => bind_ahead(addr),
            Self::Loopback | Self::Host(_) => TcpListener::bind(addr),
        }
    }

    /// Binds the listener on `port`, or on loopback when this host no
    /// longer holds the address (its interface went away since [`of`]):
    /// the proxy still serves loopback and SSH sandboxes, and the warning
    /// says why containers cannot reach it.
    ///
    /// [`of`]: Reach::of
    ///
    /// # Errors
    ///
    /// Any other bind error.
    pub fn listen(self, port: u16) -> io::Result<(Self, TcpListener)> {
        match self.bind(port) {
            Err(e) if e.kind() == io::ErrorKind::AddrNotAvailable && self != Self::Loopback => {
                tracing::warn!(
                    "the egress proxy cannot listen on {}: {e}; containers cannot reach it",
                    self.addr(port)
                );
                Ok((Self::Loopback, Self::Loopback.bind(port)?))
            }
            bound => Ok((self, bound?)),
        }
    }
}

/// The host's address on its default route: where slirp4netns and pasta
/// lead a rootless container's connections to the host. Connecting a UDP
/// socket only looks the route up; nothing is sent.
fn host_address() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    // A documentation address (RFC 5737): routed like any public one.
    socket
        .connect((std::net::Ipv4Addr::new(192, 0, 2, 1), 9))
        .ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

/// A rootful runtime's default bridge gateway.
fn gateway(runtime: &pmt_sandbox::container::Runtime) -> Option<IpAddr> {
    let (network, format) = match runtime.flavor() {
        pmt_sandbox::container::Flavor::Docker => ("bridge", "{{(index .IPAM.Config 0).Gateway}}"),
        pmt_sandbox::container::Flavor::Podman => ("podman", "{{(index .Subnets 0).Gateway}}"),
    };
    runtime
        .command()
        .args(["network", "inspect", network, "--format", format])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
}

/// Binds `addr` even while no interface holds it yet.
#[cfg(target_os = "linux")]
fn bind_ahead(addr: SocketAddr) -> io::Result<TcpListener> {
    use socket2::{Domain, Socket, Type};
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, None)?;
    socket.set_reuse_address(true)?;
    if addr.is_ipv4() {
        socket.set_freebind_v4(true)?;
    } else {
        socket.set_freebind_v6(true)?;
    }
    socket.bind(&addr.into())?;
    socket.listen(128)?;
    Ok(socket.into())
}

#[cfg(not(target_os = "linux"))]
fn bind_ahead(addr: SocketAddr) -> io::Result<TcpListener> {
    TcpListener::bind(addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rootless_reach_serves_only_the_host_and_maps_the_relay_to_it() {
        let ip = IpAddr::from([10, 1, 0, 4]);
        let reach = Reach::Host(ip);
        assert_eq!(reach.addr(9090), SocketAddr::new(ip, 9090));
        assert_eq!(reach.only_from(), Some(ip));
        assert_eq!(reach.relay_host(), Some(ip));
        let gateway = Reach::Gateway(IpAddr::from([10, 88, 0, 1]));
        assert_eq!(gateway.only_from(), None);
        assert_eq!(gateway.relay_host(), None);
    }

    /// Rootful Podman's bridge appears with its first container: the
    /// listener binds its gateway before any interface holds it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_gateway_binds_before_its_bridge_exists() {
        // TEST-NET-2: no interface of a test host holds it.
        let absent = IpAddr::from([198, 51, 100, 7]);
        assert_eq!(
            TcpListener::bind((absent, 0)).unwrap_err().kind(),
            io::ErrorKind::AddrNotAvailable
        );
        let listener = Reach::Gateway(absent).bind(0).unwrap();
        assert_eq!(listener.local_addr().unwrap().ip(), absent);
    }

    #[test]
    fn an_address_this_host_does_not_hold_falls_back_to_loopback() {
        let absent = Reach::Host(IpAddr::from([198, 51, 100, 7]));
        let (reach, listener) = absent.listen(0).unwrap();
        assert_eq!(reach, Reach::Loopback);
        assert!(listener.local_addr().unwrap().ip().is_loopback());
    }

    #[test]
    fn the_host_address_is_a_local_one() {
        let Some(ip) = host_address() else {
            return; // no default route on this host
        };
        TcpListener::bind((ip, 0)).unwrap();
    }
}
