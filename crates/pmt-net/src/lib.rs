//! Outbound networking shared by every client of a profile (providers, MCP
//! over HTTP, web tools, adapters, OTLP, update and runtime downloads):
//! the one HTTP client builder, which applies `network.force_ipv4`
//! ([`resolve`], [`http`]), and the `network` doctor group's IPv6
//! reachability check ([`doctor`]) (spec: workspace-architecture, IPv6
//! reachability check and IPv4-only outbound).

pub mod doctor;
pub mod http;
pub mod resolve;

pub use http::{OutboundPolicy, client, client_builder};
