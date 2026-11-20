//! How a proxied child reaches its egress proxy when the OS confines it
//! to a network of its own (spec: security-boundaries, Egress enforcement
//! for child processes).
//!
//! Landlock filters TCP ports, not addresses, so on Linux a child routed
//! through the egress proxy runs in its own network namespace instead:
//! its only interface is a private loopback, and the one way out is a
//! listener bound there on the proxy's port. The child hands that
//! listener to the parent before `exec`; the parent passes it to the
//! [`Relay`], which serves it with the proxy's rules until every process
//! holding the child's end of the channel has exited.
//!
//! A Windows `AppContainer` reaches loopback only within its own package,
//! so there the launcher runs a relay process in the child's container
//! (see `windows/relay.rs`): the child, holding no network capability,
//! reaches only that relay, which forwards each connection over a named
//! pipe to the launcher and on to the proxy. The [`Relay`] callback is
//! not used there.

use std::sync::Arc;

/// A proxied child's private listener, served by the egress proxy.
#[derive(Debug)]
pub struct Attached {
    /// Bound to `127.0.0.1:<proxy port>` inside the child's namespace.
    pub listener: std::net::TcpListener,
    /// Reads end-of-file once the child and every descendant holding the
    /// channel are gone: the listener is then closed.
    #[cfg(unix)]
    pub alive: std::os::unix::net::UnixStream,
}

/// Hands each [`Attached`] listener to the egress proxy.
#[derive(Clone)]
pub struct Relay {
    serve: Arc<dyn Fn(Attached) + Send + Sync>,
    inbound: Option<Inbound>,
}

impl Relay {
    /// A relay calling `serve` with each child's listener.
    pub fn new(serve: impl Fn(Attached) + Send + Sync + 'static) -> Self {
        Self {
            serve: Arc::new(serve),
            inbound: None,
        }
    }

    /// The relay that also lets the parent connect to a port the child
    /// listens on (see [`Inbound`]).
    #[must_use]
    pub fn with_inbound(mut self, inbound: Inbound) -> Self {
        self.inbound = Some(inbound);
        self
    }

    /// The parent's way in, when asked for.
    #[must_use]
    pub fn inbound(&self) -> Option<&Inbound> {
        self.inbound.as_ref()
    }

    /// Serves `attached`.
    pub fn serve(&self, attached: Attached) {
        (self.serve)(attached);
    }
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Relay")
    }
}

/// Two policies are equal when they hand children to the same relay.
impl PartialEq for Relay {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.serve, &other.serve)
    }
}

impl Eq for Relay {}

/// The parent's way to a loopback port a proxied child listens on (a
/// browser serving DevTools on TCP, such as Lightpanda).
///
/// Where the child runs in a network namespace of its own (Linux), its
/// loopback is not the host's: the child creates a few unconnected TCP
/// sockets there before `exec` and hands them over, and a socket keeps the
/// namespace it was created in, so connecting one reaches the child's
/// loopback. Elsewhere (and where no namespace was made) the port is on
/// the host's loopback, allowed to the child by its `net.listen` grant.
#[derive(Debug, Clone, Default)]
pub struct Inbound {
    state: Arc<(std::sync::Mutex<InboundState>, std::sync::Condvar)>,
}

#[derive(Debug, Default)]
struct InboundState {
    /// The child was put in its own network namespace.
    isolated: bool,
    /// Sockets created in that namespace, not yet used.
    #[cfg(target_os = "linux")]
    sockets: Vec<std::os::fd::OwnedFd>,
    /// The child sent everything it will send.
    closed: bool,
}

/// How many sockets a namespaced child hands over: each DevTools
/// connection (the first, and reconnects) uses one.
pub const INBOUND_SOCKETS: usize = 16;

impl Inbound {
    /// A fresh way in (nothing connected yet).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, InboundState> {
        self.state
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Marks the child as namespaced: connections use its sockets.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn isolate(&self) {
        self.lock().isolated = true;
    }

    /// Keeps a socket the child created in its namespace.
    #[cfg(target_os = "linux")]
    pub(crate) fn add(&self, socket: std::os::fd::OwnedFd) {
        self.lock().sockets.push(socket);
        self.state.1.notify_all();
    }

    /// The child will send no more sockets.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn close(&self) {
        self.lock().closed = true;
        self.state.1.notify_all();
    }

    /// Connects to the child's `127.0.0.1:port`.
    ///
    /// # Errors
    ///
    /// The connection is refused, or every socket into the child's
    /// namespace was used.
    pub fn connect(&self, port: u16) -> std::io::Result<std::net::TcpStream> {
        let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        match self.take()? {
            None => std::net::TcpStream::connect(address),
            #[cfg(target_os = "linux")]
            Some(socket) => {
                let socket = socket2::Socket::from(socket);
                socket.connect(&address.into())?;
                Ok(socket.into())
            }
            #[cfg(not(target_os = "linux"))]
            Some(never) => match never {},
        }
    }

    /// A socket into the child's namespace (`None`: use the host's
    /// loopback), waiting briefly for the child to send them.
    #[cfg(target_os = "linux")]
    fn take(&self) -> std::io::Result<Option<std::os::fd::OwnedFd>> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut state = self.lock();
        loop {
            if !state.isolated {
                return Ok(None);
            }
            if let Some(socket) = state.sockets.pop() {
                return Ok(Some(socket));
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if state.closed || left.is_zero() {
                return Err(std::io::Error::other(
                    "no socket into the browser's network namespace is left",
                ));
            }
            state = self
                .state
                .1
                .wait_timeout(state, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[allow(
        clippy::unnecessary_wraps,
        clippy::unused_self,
        reason = "one signature for every platform"
    )]
    fn take(&self) -> std::io::Result<Option<std::convert::Infallible>> {
        Ok(None)
    }
}
