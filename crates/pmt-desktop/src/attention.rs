//! What the shell does while the windows are in the background (spec:
//! desktop-app, Closing windows does not stop the agent; Approval
//! notifications never approve).
//!
//! An approval for an unfocused window becomes an OS notification naming
//! the session and the tool. The notification carries no actions: the OS
//! activates the app when it is clicked, and the shell then hands the
//! window the request to focus ([`Notices::activated`]). Nothing here can
//! answer a request; the card in the window is the only place that does.
//!
//! The tray menu lists the pending approvals and the running sessions the
//! window reports, so the user sees them with every window closed.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// One approval the window was notified of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    /// The server request id.
    pub request_id: String,
    /// Its session.
    pub session_id: String,
    /// The session's title, when it has one.
    #[serde(default)]
    pub session_title: Option<String>,
    /// The tool asking.
    pub tool: String,
}

impl Approval {
    /// The notification's title and body.
    #[must_use]
    pub fn notification(&self) -> (String, String) {
        let session = self
            .session_title
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(&self.session_id);
        (
            format!("Approval needed: {}", self.tool),
            format!(
                "{session} is waiting for your approval to run {}.",
                self.tool
            ),
        )
    }
}

/// The approval notifications not yet acted on.
#[derive(Debug, Default)]
pub struct Notices {
    latest: Mutex<Option<Approval>>,
}

impl Notices {
    /// A notification for `approval` was posted.
    pub fn posted(&self, approval: Approval) {
        *self.lock() = Some(approval);
    }

    /// The request was answered or settled elsewhere: clicking its
    /// notification later opens nothing.
    pub fn resolved(&self, request_id: &str) {
        let mut latest = self.lock();
        if latest.as_ref().is_some_and(|a| a.request_id == request_id) {
            *latest = None;
        }
    }

    /// The app was activated (a notification click): the approval whose
    /// card the window should focus, once.
    pub fn activated(&self) -> Option<Approval> {
        self.lock().take()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Approval>> {
        self.latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One tray menu line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrayItem {
    /// The session it opens.
    pub session_id: String,
    /// What the line says.
    pub label: String,
}

/// What the tray menu shows.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrayState {
    /// Sessions waiting for an approval.
    #[serde(default)]
    pub approvals: Vec<TrayItem>,
    /// Sessions with a running turn.
    #[serde(default)]
    pub running: Vec<TrayItem>,
}

/// One entry of the rendered tray menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    /// A heading (disabled).
    Heading(String),
    /// Opens a session in the main window (`open:<session id>`).
    Open {
        /// The menu item id.
        id: String,
        /// Its text.
        label: String,
    },
    /// Shows the main window.
    Show,
    /// Quits the app (never the daemon).
    Quit,
}

impl TrayState {
    /// The menu, top to bottom.
    #[must_use]
    pub fn menu(&self) -> Vec<MenuEntry> {
        let mut menu = Vec::new();
        let mut section = |title: String, items: &[TrayItem]| {
            menu.push(MenuEntry::Heading(title));
            menu.extend(items.iter().map(|i| MenuEntry::Open {
                id: format!("open:{}", i.session_id),
                label: i.label.clone(),
            }));
        };
        section(
            format!("Pending approvals ({})", self.approvals.len()),
            &self.approvals,
        );
        section(
            format!("Running sessions ({})", self.running.len()),
            &self.running,
        );
        menu.push(MenuEntry::Show);
        menu.push(MenuEntry::Quit);
        menu
    }
}

#[cfg(test)]
mod tests {
    use super::{Approval, MenuEntry, Notices, TrayItem, TrayState};

    fn approval(id: &str) -> Approval {
        Approval {
            request_id: id.into(),
            session_id: "ses_1".into(),
            session_title: Some("Deploy".into()),
            tool: "terminal".into(),
        }
    }

    #[test]
    fn a_notification_names_the_session_and_the_tool() {
        let (title, body) = approval("srq-1").notification();
        assert_eq!(title, "Approval needed: terminal");
        assert!(body.contains("Deploy") && body.contains("terminal"));
    }

    #[test]
    fn activation_hands_over_the_latest_unanswered_approval_once() {
        let notices = Notices::default();
        notices.posted(approval("srq-1"));
        assert_eq!(notices.activated().unwrap().request_id, "srq-1");
        assert_eq!(notices.activated(), None);
        notices.posted(approval("srq-2"));
        notices.resolved("srq-2");
        assert_eq!(notices.activated(), None, "answered elsewhere");
    }

    #[test]
    fn the_tray_lists_approvals_and_running_sessions() {
        let state = TrayState {
            approvals: vec![TrayItem {
                session_id: "ses_1".into(),
                label: "Deploy: terminal".into(),
            }],
            running: vec![],
        };
        let menu = state.menu();
        assert_eq!(menu[0], MenuEntry::Heading("Pending approvals (1)".into()));
        assert!(matches!(&menu[1], MenuEntry::Open { id, .. } if id == "open:ses_1"));
        assert_eq!(menu.last(), Some(&MenuEntry::Quit));
    }
}
