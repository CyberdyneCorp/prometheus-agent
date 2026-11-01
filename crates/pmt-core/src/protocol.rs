//! The local client protocol version, identified everywhere as
//! `<major>.<minor>` (spec: workspace-architecture, Protocol handshake).
//! The protocol itself arrives in M2; `prometheus version` reports it now
//! so clients and the daemon share one constant from the start.

use std::fmt;

/// A `<major>.<minor>` protocol version. Minor versions only add optional
/// fields, methods and events; a different major is incompatible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolVersion {
    /// Incompatible revisions.
    pub major: u16,
    /// Backward-compatible additions.
    pub minor: u16,
}

/// The protocol this build speaks.
pub const RPC_PROTOCOL: ProtocolVersion = ProtocolVersion { major: 1, minor: 0 };

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

#[cfg(test)]
mod tests {
    use super::RPC_PROTOCOL;

    #[test]
    fn displays_as_major_dot_minor() {
        let text = RPC_PROTOCOL.to_string();
        let (major, minor) = text.split_once('.').expect("one dot");
        assert_eq!(major.parse::<u16>().unwrap(), RPC_PROTOCOL.major);
        assert_eq!(minor.parse::<u16>().unwrap(), RPC_PROTOCOL.minor);
    }
}
