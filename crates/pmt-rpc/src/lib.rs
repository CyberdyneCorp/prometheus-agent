//! The local client protocol shared by the daemon and every client (CLI,
//! TUI, desktop, dashboard, ACP): JSON-RPC 2.0 over a loopback WebSocket
//! (spec: workspace-architecture, JSON-RPC framing and error codes and the
//! requirements after it).
//!
//! - [`codes`]: the error-code and close-code tables;
//! - [`frame`]: parsing and building frames;
//! - [`catalog`]: every method, server request and event name with its
//!   owning capability and typed contract, validation, `schema/rpc.json`;
//! - [`typescript`]: the TypeScript bindings rendered from that schema;
//! - [`discovery`], [`control`] and [`client`]: finding, authenticating
//!   to and talking to a running daemon.
//!
//! The server lives in `pmt-gateway`.

pub mod catalog;
pub mod client;
pub mod codes;
pub mod control;
pub mod discovery;
pub mod frame;
pub mod reference;
pub mod typescript;
pub mod validate;

pub use catalog::contracts;
pub use catalog::{Catalog, Kind};
pub use frame::{Id, RpcError};
