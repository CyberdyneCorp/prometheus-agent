//! The adapter's connection to the gateway daemon (spec: acp-adapter, ACP
//! process is a daemon viewer; Daemon connection is bound to the launch
//! profile): made lazily on the first session call (once the editor said
//! what it can do, which decides the client features advertised), re-made
//! on the next call after it is lost.

use std::pin::Pin;

use pmt_core::error::Error;
use pmt_rpc::client::{Client, Inbox};

/// Future of a connection attempt.
pub type ConnectFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(Client, Inbox), Error>> + Send + 'a>>;

/// Makes the adapter's daemon connection: finds (or starts, when
/// `daemon.autostart` allows) the home's daemon, gets a ticket granting
/// only the launch profile and performs `rpc.hello` as an `acp` client
/// with `features` (the editor-backed ones, [`pmt_rpc::contracts::EDITOR_FS`]
/// and [`pmt_rpc::contracts::EDITOR_TERMINAL`]).
pub trait Connector: Send + Sync {
    /// Connects.
    fn connect<'a>(&'a self, features: &'a [String]) -> ConnectFuture<'a>;
}

/// The error code of a daemon that does not serve the launch profile.
pub(crate) const PROFILE_MISMATCH: &str = "profile_mismatch";

/// A live connection and the daemon instance it reached.
#[derive(Debug, Clone)]
pub(crate) struct Conn {
    pub(crate) client: Client,
    pub(crate) instance: String,
}

impl Conn {
    /// Whether the connection still works.
    pub(crate) fn live(&self) -> bool {
        self.client.closed().is_none()
    }
}
