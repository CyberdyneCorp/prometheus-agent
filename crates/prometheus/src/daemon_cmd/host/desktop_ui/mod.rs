//! The daemon side of the desktop UI toolset, the onboarding beat and the
//! `project` tool (spec: desktop-app, Desktop UI toolset contract and the
//! requirements after it; projects, Project tool for GUI sessions).
//!
//! A window tool becomes the server request `ui.<tool>`, sent only to the
//! session's drivers whose client advertised the `desktop_ui` feature (an
//! open desktop window), with a 30 s deadline (300 s for picker, tour and
//! onboarding cards). The daemon validates the arguments first, so a
//! refused call never reaches the window, applies the URL safety policy
//! to preview navigations, asks the user before a password field is typed
//! into and redacts secrets from what the window reads back.
//!
//! The session's way to its clients is the link of its turn: a turn binds
//! it when it starts (tools run only inside turns) and the session's
//! unload drops it.

mod check;
mod drive;
mod onboarding;
mod processes;
mod project;
mod reactions;
mod window;

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};

use pmt_agent::tools::{DesktopPort, DesktopPorts};
use pmt_agent::turn::model::BoxFuture;
use pmt_config::ProfileScope;
use pmt_core::cancel::CancelToken;
use pmt_core::id::SessionId;
use pmt_core::secret::Redactor;
use pmt_gateway::Link;
use pmt_store::Store;
use pmt_tools::ToolResult;
use pmt_tools::result::Status;
use serde_json::{Value, json};

use super::Loaded;
use crate::chat::session::tools::SwappableTools;

pub(super) use self::onboarding::ensure_chat;
pub(super) use self::processes::{list as processes, log as process_log};
pub(super) use self::reactions::reactions_of;

/// A session's way to its desktop clients and its tool port.
struct Bound {
    link: Link,
    tools: Weak<SwappableTools>,
}

static BOUND: LazyLock<Mutex<HashMap<String, Bound>>> = LazyLock::new(Mutex::default);

fn bound() -> std::sync::MutexGuard<'static, HashMap<String, Bound>> {
    BOUND.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Makes `session`'s clients reachable through `link` (its turn's) and
/// lets the `project` tool move its working directory.
pub(super) fn bind(id: SessionId, session: &Arc<Loaded>, link: &Link) {
    let weak = Arc::downgrade(session);
    session.live.tools.set_mover(Arc::new(move |cwd| {
        let weak = weak.clone();
        Box::pin(async move {
            let Some(loaded) = weak.upgrade() else {
                return Ok(());
            };
            loaded.live.retarget(&loaded.factory, &cwd).await
        })
    }));
    bound().insert(
        id.to_string(),
        Bound {
            link: link.clone(),
            tools: Arc::downgrade(&session.live.tools),
        },
    );
}

/// Forgets an unloaded session.
pub(super) fn unbind(id: SessionId) {
    bound().remove(&id.to_string());
}

/// The desktop tools of one profile's sessions.
pub(super) struct UiPorts {
    scope: ProfileScope,
    store: Arc<Store>,
}

impl UiPorts {
    pub(super) fn new(scope: ProfileScope, store: Arc<Store>) -> Self {
        Self { scope, store }
    }
}

impl DesktopPorts for UiPorts {
    fn port(&self, session: SessionId) -> Arc<dyn DesktopPort> {
        Arc::new(UiPort {
            session,
            redactor: self.scope.secrets().redactor().unwrap_or_default(),
            scope: self.scope.clone(),
            store: Arc::clone(&self.store),
        })
    }
}

/// One session's desktop tools.
pub(super) struct UiPort {
    session: SessionId,
    scope: ProfileScope,
    store: Arc<Store>,
    redactor: Redactor,
}

impl DesktopPort for UiPort {
    fn call<'a>(
        &'a self,
        name: &'a str,
        arguments: Value,
        _cancel: &'a CancelToken,
    ) -> BoxFuture<'a, ToolResult> {
        // The dispatcher races the call against the turn's cancel token;
        // dropping it cancels an open window request.
        Box::pin(async move {
            match name {
                "react_to_message" => reactions::react(self, &arguments),
                "project" => project::call(self, &arguments),
                "setup_beat" => onboarding::beat(self, arguments).await,
                "drive_preview" => drive::call(self, arguments).await,
                _ => window::call(self, name, arguments).await,
            }
        })
    }
}

impl UiPort {
    /// The session's link, when a turn bound it.
    fn link(&self) -> Option<Link> {
        bound()
            .get(&self.session.to_string())
            .map(|b| b.link.clone())
    }

    /// The session's tool port, while it is loaded.
    fn tools(&self) -> Option<Arc<SwappableTools>> {
        bound()
            .get(&self.session.to_string())
            .and_then(|b| b.tools.upgrade())
    }
}

/// A tool's answer: the window's own JSON, an error when it says
/// `success: false`.
fn answer(value: &Value) -> ToolResult {
    let failed = value.get("success").and_then(Value::as_bool) == Some(false);
    let status = if failed { Status::Error } else { Status::Ok };
    ToolResult::text(status, value.to_string())
}

/// The structured failure every desktop tool answers with.
fn fail(error: impl Into<String>) -> ToolResult {
    answer(&json!({"success": false, "error": error.into()}))
}
