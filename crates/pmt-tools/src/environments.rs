//! Where built-in tool handlers find a session's execution environment.
//!
//! Tool handlers are shared registry entries (one per process), but the
//! terminal, the file tools and the web tools act on per-session state.
//! Session start attaches a [`SessionEnv`] under the session id; the
//! handlers look it up by [`crate::CallContext::session_id`] and the
//! [`Attachment`] detaches it when the session ends.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, PoisonError, RwLock};

use crate::browser::BrowserExec;
use crate::descriptor::{CallContext, ToolFailure};
use crate::files::FileSession;
use crate::terminal::LocalTerminal;
use crate::vision::VisionSession;
use crate::web::WebSession;

/// One session's execution environment. Each part is optional: a session
/// without a terminal (no `terminal` toolset) attaches none.
#[derive(Default)]
pub struct SessionEnv {
    /// The file tools' state.
    pub files: Option<Arc<FileSession>>,
    /// The `local` backend shell.
    pub terminal: Option<Arc<LocalTerminal>>,
    /// The web tools' backends and fetcher.
    pub web: Option<Arc<WebSession>>,
    /// `vision_analyze`'s routes.
    pub vision: Option<Arc<VisionSession>>,
    /// `browser_exec`'s browsers and interpreter.
    pub browser: Option<Arc<BrowserExec>>,
}

/// The attached environments of a process, by session id.
#[derive(Default)]
pub struct Environments {
    sessions: RwLock<HashMap<String, Arc<SessionEnv>>>,
}

static PROCESS: LazyLock<Arc<Environments>> = LazyLock::new(Arc::default);

impl Environments {
    /// The process-wide table the built-in tools use.
    #[must_use]
    pub fn process() -> Arc<Self> {
        Arc::clone(&PROCESS)
    }

    /// Attaches `env` to `session_id` until the returned [`Attachment`] is
    /// dropped.
    #[must_use]
    pub fn attach(self: &Arc<Self>, session_id: &str, env: SessionEnv) -> Attachment {
        self.sessions
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(session_id.to_owned(), Arc::new(env));
        Attachment {
            table: Arc::clone(self),
            session_id: session_id.to_owned(),
        }
    }

    /// The environment of the calling session.
    ///
    /// # Errors
    ///
    /// A `unavailable` failure when nothing is attached.
    pub fn of(&self, context: &CallContext) -> Result<Arc<SessionEnv>, ToolFailure> {
        self.sessions
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&context.session_id)
            .cloned()
            .ok_or_else(|| {
                ToolFailure::new(
                    "unavailable",
                    format!(
                        "no execution environment is attached to session {}",
                        context.session_id
                    ),
                )
            })
    }
}

/// Keeps a session's environment attached; dropping it detaches.
pub struct Attachment {
    table: Arc<Environments>,
    session_id: String,
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.table
            .sessions
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.session_id);
    }
}

/// `part` of the calling session's environment, or why it is missing.
pub(crate) fn part<T>(
    table: &Environments,
    context: &CallContext,
    name: &str,
    pick: impl FnOnce(&SessionEnv) -> Option<Arc<T>>,
) -> Result<Arc<T>, ToolFailure> {
    let env = table.of(context)?;
    pick(&env).ok_or_else(|| {
        ToolFailure::new(
            "unavailable",
            format!("session {} has no {name} environment", context.session_id),
        )
    })
}
