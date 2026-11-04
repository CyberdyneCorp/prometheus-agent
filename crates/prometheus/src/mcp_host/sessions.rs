//! The open sessions as MCP servers reach them (spec: plugin-system, MCP
//! sampling; MCP elicitation; MCP tool costs and spend limits; tool-system,
//! MCP media results; MCP tool list changes respect the frozen surface).
//!
//! A profile's servers are shared by its sessions; what a server asks or
//! returns for one tool call (an elicitation, a sampling request, a media
//! block, a costed call) goes to the session that made the call. Each open
//! session registers itself here ([`register`]) for as long as it lives;
//! a call made for a session that is not registered (one-shot `mcp`
//! commands) gets the answers of a session no one attends.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use pmt_mcp::host::{ClientHost, ElicitAnswer, Elicitation, HostFuture, Sampled, Sampling};

/// One open session, as MCP servers reach it.
pub trait McpSession: Send + Sync {
    /// The session's profile directory.
    fn profile_dir(&self) -> PathBuf;

    /// Stores a media block in the profile's blob store; answers its path.
    ///
    /// # Errors
    ///
    /// Why it was not stored.
    fn store_media(&self, bytes: &[u8], mime: &str) -> Result<String, String>;

    /// Whether the session's route accepts images.
    fn accepts_images(&self) -> bool;

    /// Shows a one-line notice.
    fn notice(&self, text: String);

    /// Asks the session's person.
    fn elicit(&self, request: Elicitation) -> HostFuture<'_, ElicitAnswer>;

    /// Runs a sampling request on the session's route.
    fn sample(&self, request: Sampling) -> HostFuture<'_, Result<Sampled, String>>;

    /// What the conversation spent on `server`.
    fn spent(&self, server: &str) -> f64;

    /// Adds to it.
    fn add_spent(&self, server: &str, amount: f64);
}

type Sessions = Mutex<HashMap<String, Weak<dyn McpSession>>>;

fn sessions() -> &'static Sessions {
    static SESSIONS: OnceLock<Sessions> = OnceLock::new();
    SESSIONS.get_or_init(Sessions::default)
}

fn find(session: &str) -> Option<Arc<dyn McpSession>> {
    sessions()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(session)
        .and_then(Weak::upgrade)
}

/// Keeps a session reachable until dropped.
pub struct Registration {
    session: String,
    _session: Arc<dyn McpSession>,
}

impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registration")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        sessions()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.session);
    }
}

/// Makes `session` (id `id`) reachable by MCP servers.
#[must_use]
pub fn register(id: &str, session: Arc<dyn McpSession>) -> Registration {
    sessions()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id.to_owned(), Arc::downgrade(&session));
    Registration {
        session: id.to_owned(),
        _session: session,
    }
}

/// The host of one profile's servers: routes to the registered sessions.
pub struct SessionsHost {
    /// The profile directory (tool list notices go to its sessions only).
    pub profile_dir: PathBuf,
}

impl ClientHost for SessionsHost {
    fn store_media(&self, session: &str, bytes: &[u8], mime: &str) -> Result<String, String> {
        find(session)
            .ok_or_else(|| "no open session to store it for".to_owned())?
            .store_media(bytes, mime)
    }

    fn accepts_images(&self, session: &str) -> bool {
        find(session).is_some_and(|s| s.accepts_images())
    }

    fn tools_changed(&self, server: &str) {
        let open: Vec<Arc<dyn McpSession>> = sessions()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        for session in open.iter().filter(|s| s.profile_dir() == self.profile_dir) {
            session.notice(format!(
                "MCP server '{server}' changed its tools; they apply from the next session (or /mcp reload --now)"
            ));
        }
    }

    fn elicit(&self, session: &str, request: Elicitation) -> HostFuture<'_, ElicitAnswer> {
        let found = find(session);
        Box::pin(async move {
            match found {
                Some(session) => session.elicit(request).await,
                None => ElicitAnswer::Decline,
            }
        })
    }

    fn sample(&self, session: &str, request: Sampling) -> HostFuture<'_, Result<Sampled, String>> {
        let found = find(session);
        Box::pin(async move {
            match found {
                Some(session) => session.sample(request).await,
                None => Err("sampling needs an open session".to_owned()),
            }
        })
    }

    fn spent(&self, session: &str, server: &str) -> f64 {
        find(session).map_or(0.0, |s| s.spent(server))
    }

    fn add_spent(&self, session: &str, server: &str, amount: f64) {
        if let Some(session) = find(session) {
            session.add_spent(server, amount);
        }
    }
}
