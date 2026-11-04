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
pub struct Relay(Arc<dyn Fn(Attached) + Send + Sync>);

impl Relay {
    /// A relay calling `serve` with each child's listener.
    pub fn new(serve: impl Fn(Attached) + Send + Sync + 'static) -> Self {
        Self(Arc::new(serve))
    }

    /// Serves `attached`.
    pub fn serve(&self, attached: Attached) {
        (self.0)(attached);
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
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Relay {}
