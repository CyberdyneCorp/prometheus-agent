//! Hosted connectors (spec: connectors): app integrations (Gmail, Notion,
//! Linear...) whose tools run on the Nous connector gateway, reached with
//! the profile's Nous sign-in.
//!
//! - [`client`]: the gateway's HTTP client.
//! - [`remote`]: connector tools behind the tool-search bridge, never in
//!   the tool array.
//! - [`manage`]: the `manage_connections` tool.
//!
//! Nothing here creates a Nous identity: without a sign-in the session
//! simply has no connectors.

pub mod client;
pub mod manage;
pub mod names;
pub mod remote;
pub mod wire;

#[cfg(feature = "connectors-fake")]
pub mod fake;

pub use self::client::{Gateway, GatewayError, TokenSource};
pub use self::remote::Connectors;
