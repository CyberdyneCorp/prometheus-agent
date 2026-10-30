//! Outbound and inbound networking shared by every client and surface of a
//! profile:
//!
//! - [`http`]: the one HTTP client builder (providers, MCP over HTTP, web
//!   tools, adapters, OTLP, update and runtime downloads), applying
//!   `network.force_ipv4` ([`resolve`]) and platform-verified TLS ([`tls`]);
//! - [`destination`] and [`fetch`]: the private/metadata destination policy
//!   and the SSRF-safe fetcher for model- or remote-supplied URLs;
//! - [`listen`]: loopback-first listeners and the fail-closed dispatch gate
//!   of external surfaces;
//! - [`doctor`]: the `network` doctor group's IPv6 reachability check.
//!
//! (specs: workspace-architecture, IPv6 reachability check and IPv4-only
//! outbound; security-boundaries, Private and metadata destinations,
//! In-process fetch SSRF protection, Platform trust store for TLS, External
//! surfaces fail closed.)

pub mod destination;
pub mod doctor;
pub mod fetch;
pub mod http;
pub mod listen;
pub mod resolve;
pub mod tls;

pub use destination::{DestinationPolicy, PrivateGrant};
pub use fetch::Fetcher;
pub use http::{OutboundPolicy, client, client_builder};
