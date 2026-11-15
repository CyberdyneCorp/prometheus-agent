//! Peer gateways (spec: agent-interop): other machines' Prometheus
//! gateways reached over their API server, for direct messages, remote
//! runs and the union roster.

pub(crate) mod client;
pub(crate) mod cmd;
