//! Outbound agent interop (spec: agent-interop): the `a2a_*` tools over the
//! A2A client, and the egress-checked HTTP they and the peer gateway
//! commands share, and the sending side of the desktop relay.

pub(crate) mod orchestrate;
pub mod relay;
pub mod tool;
pub mod transport;
