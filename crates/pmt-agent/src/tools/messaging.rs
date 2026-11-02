//! The seam between the turn loop and the messaging gateway (spec:
//! agent-interop, `send_message` tool): the daemon implements
//! [`MessagingPort`] for its sessions; a session without one (in-process
//! chat) answers that the tool needs the gateway daemon.

use pmt_core::id::SessionId;
use pmt_tools::ToolResult;
use serde_json::Value;

use crate::turn::model::BoxFuture;

/// The gateway, as `send_message` reaches it.
pub trait MessagingPort: Send + Sync {
    /// Runs one `send_message` call of `session`.
    fn call(&self, session: SessionId, arguments: Value) -> BoxFuture<'_, ToolResult>;
}

/// Whether the session may read a path (its effective `fs.read` grants).
pub type ReadCheck = std::sync::Arc<dyn Fn(&std::path::Path) -> bool + Send + Sync>;

/// Builds each session's port (the daemon installs one per profile).
pub trait MessagingPorts: Send + Sync {
    /// The port of `session`, whose reads `may_read` decides.
    fn port(&self, session: SessionId, may_read: ReadCheck) -> std::sync::Arc<dyn MessagingPort>;
}

/// What `send_message` answers without a gateway.
pub const NO_GATEWAY: &str = "send_message needs the gateway daemon: start it with \
    'prometheus daemon start' and use the session through it";
