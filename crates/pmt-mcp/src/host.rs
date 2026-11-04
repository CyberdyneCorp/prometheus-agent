//! What the MCP client needs from the program around it (spec:
//! plugin-system, MCP sampling; MCP elicitation; MCP tool costs and spend
//! limits; tool-system, MCP media results; MCP tool list changes respect
//! the frozen surface).
//!
//! A profile's servers are shared by its sessions, so whatever a server
//! asks or returns that belongs to one session (a media block, an
//! elicitation, a sampling request, a costed call) is routed to the
//! session of the tool call in flight through this port. [`NoHost`]
//! answers like a process with no session: media cannot be stored,
//! elicitations are declined and sampling is refused.

use std::future::Future;
use std::pin::Pin;

use serde_json::{Map, Value};

/// A boxed future of the port.
pub type HostFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A form-mode elicitation of a server.
#[derive(Debug, Clone, PartialEq)]
pub struct Elicitation {
    /// The asking server.
    pub server: String,
    /// What the server asks.
    pub message: String,
    /// The requested schema (an object of primitive properties).
    pub schema: Value,
}

/// How the person answered an elicitation.
#[derive(Debug, Clone, PartialEq)]
pub enum ElicitAnswer {
    /// Answered once, with these values.
    Accept(Map<String, Value>),
    /// Declined (also: no one can answer, timed out).
    Decline,
    /// Dismissed.
    Cancel,
}

/// One sampling request, after the server's limits admitted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sampling {
    /// The asking server (charged as `mcp:<server>`).
    pub server: String,
    /// The system prompt.
    pub system: Option<String>,
    /// `(role, text)` pairs (`user` or `assistant`).
    pub messages: Vec<(String, String)>,
    /// The model to use instead of the session's.
    pub model: Option<String>,
    /// The answer size cap.
    pub max_tokens: u32,
}

/// A sampling answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sampled {
    /// The model that answered.
    pub model: String,
    /// Its text.
    pub text: String,
}

/// The program's side of the MCP client, shared by a profile's servers.
pub trait ClientHost: Send + Sync {
    /// Stores one media block of a result of a call made for `session`;
    /// answers the stored file's path.
    ///
    /// # Errors
    ///
    /// Why it could not be stored.
    fn store_media(&self, session: &str, bytes: &[u8], mime: &str) -> Result<String, String> {
        let _ = (session, bytes, mime);
        Err("no media store".to_owned())
    }

    /// Whether `session`'s route accepts images in tool results.
    fn accepts_images(&self, session: &str) -> bool {
        let _ = session;
        false
    }

    /// `server`'s tool list changed: running sessions show a notice (their
    /// tool array stays as it is).
    fn tools_changed(&self, server: &str) {
        let _ = server;
    }

    /// Asks `session`'s person; answers [`ElicitAnswer::Decline`] at once
    /// when no one can answer.
    fn elicit(&self, session: &str, request: Elicitation) -> HostFuture<'_, ElicitAnswer> {
        let _ = (session, request);
        Box::pin(async { ElicitAnswer::Decline })
    }

    /// Runs one sampling request on `session`'s route, charged to its
    /// ledger.
    fn sample(&self, session: &str, request: Sampling) -> HostFuture<'_, Result<Sampled, String>> {
        let _ = (session, request);
        Box::pin(async { Err("sampling needs a session".to_owned()) })
    }

    /// What `session`'s conversation spent on `server`'s costed tools.
    fn spent(&self, session: &str, server: &str) -> f64 {
        let _ = (session, server);
        0.0
    }

    /// Adds `amount` to that total.
    fn add_spent(&self, session: &str, server: &str, amount: f64) {
        let _ = (session, server, amount);
    }
}

/// A host with no sessions.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHost;

impl ClientHost for NoHost {}
