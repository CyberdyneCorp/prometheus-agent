//! The gateway daemon: one per Prometheus home, owning sessions, agents and
//! schedulers, the loopback JSON-RPC listener every local client attaches
//! to, and (with the messaging adapters, in `pmt-adapters-*` crates) the
//! messaging outbox.
//!
//! - [`daemon::run`]: the daemon's lifetime (bind, publish, serve, drain);
//! - [`control`]: the control channel (tickets, status, stop);
//! - [`connection`]: one client connection (upgrade checks, handshake,
//!   framing, liveness);
//! - [`router`] and [`methods`]: typed method dispatch and the built-in
//!   session methods with their profile and ownership checks;
//! - [`hub`]: attachments, per-session event logs with replay, and
//!   server-to-client requests;
//! - [`host`]: the agent side the daemon serves sessions with.
//!
//! The protocol itself (frames, codes, catalog, client) is `pmt-rpc`.

pub mod connection;
pub mod control;
pub mod daemon;
pub mod host;
pub mod hub;
pub mod methods;
pub mod router;
pub mod server;
pub mod tickets;

pub use daemon::{Exit, run};
pub use host::{Host, HostFuture, Link};
pub use server::{Identity, Server, ServerOptions};
