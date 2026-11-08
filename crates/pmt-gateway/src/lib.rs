//! The gateway daemon: one per Prometheus home, owning sessions, agents and
//! schedulers, the loopback JSON-RPC listener every local client attaches
//! to, and (with the messaging adapters, in `pmt-adapters-*` crates) the
//! messaging outbox.
//!
//! - [`daemon::run`]: the daemon's lifetime (bind, publish, serve, drain);
//! - [`control`]: the control channel (tickets, status, stop);
//! - [`connection`]: one client connection (upgrade checks, handshake,
//!   framing, liveness);
//! - [`router`], [`methods`] and [`discovery`]: typed method dispatch,
//!   the built-in session methods with their profile and attachment checks,
//!   and the read-only discovery methods;
//! - [`hub`]: attachments, per-session event logs with replay, and
//!   server-to-client requests;
//! - [`host`]: the agent side the daemon serves sessions with;
//! - [`messaging`]: the messaging gateway (adapters, conversations,
//!   authorization and pairing, the delivery outbox, crash recovery).
//!
//! The protocol itself (frames, codes, catalog, client) is `pmt-rpc`.

pub mod connection;
pub mod control;
pub mod daemon;
pub mod discovery;
pub mod host;
pub mod hub;
pub mod messaging;
pub mod methods;
pub mod router;
pub mod server;
pub mod slots;
pub mod tickets;
pub mod upkeep;
pub mod vault;
pub mod worktree;

pub use daemon::{Exit, run};
pub use host::{Author, Host, HostFuture, Link};
pub use server::{Identity, Server, ServerOptions};
