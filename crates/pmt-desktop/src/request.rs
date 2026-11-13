//! The native command allowlist (spec: desktop-app, Native command
//! allowlist). The webview reaches the shell through one entry point that
//! takes exactly these typed requests; anything else fails to deserialize
//! and is rejected. The machine integration behind them is a [`Native`]
//! implementation (the Tauri app's, or a fake in tests).

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::dial::DialPurpose;

fn rpc_path() -> String {
    crate::endpoint::RPC_PATH.to_owned()
}

/// One request from a window.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(
    tag = "command",
    content = "args",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Request {
    /// Ensure the daemon and attach to it (the `local` connection).
    Attach {
        /// The selected profile.
        #[serde(default)]
        profile: Option<String>,
    },
    /// Restart a stale daemon after the user agreed, then attach.
    RestartDaemon {
        /// The selected profile.
        #[serde(default)]
        profile: Option<String>,
    },
    /// A one-use ticket for one WebSocket dial of this window.
    PrepareDial {
        /// The window's profile.
        profile: String,
        /// The target path.
        #[serde(default = "rpc_path")]
        path: String,
        /// `interactive` (default) or `http`.
        #[serde(default)]
        purpose: DialPurpose,
    },
    /// The dial opened or failed.
    SettleDial {
        /// The dial id.
        dial: String,
    },
    /// Open an external link (after a trusted primary-button click).
    OpenExternal {
        /// The link.
        url: String,
        /// The session workspace, for local paths.
        #[serde(default)]
        workspace: Option<String>,
    },
    /// Open a local file with the guard.
    OpenFile {
        /// The file.
        path: String,
        /// The session workspace.
        #[serde(default)]
        workspace: Option<String>,
    },
    /// Show a file in the OS file manager.
    RevealInFolder {
        /// The file.
        path: String,
        /// The session workspace.
        #[serde(default)]
        workspace: Option<String>,
    },
    /// The OS file picker.
    PickFile {
        /// The dialog title.
        #[serde(default)]
        title: Option<String>,
    },
    /// The OS directory picker.
    PickDirectory {
        /// The dialog title.
        #[serde(default)]
        title: Option<String>,
    },
    /// An OS notification.
    Notify {
        /// Its title.
        title: String,
        /// Its text.
        body: String,
    },
    /// The dock or taskbar badge.
    SetBadge {
        /// The count; none clears it.
        #[serde(default)]
        count: Option<u32>,
    },
    /// A global shortcut.
    RegisterHotkey {
        /// The chord (`CommandOrControl+Shift+Space`).
        chord: String,
    },
    /// The clipboard's text, on an explicit user action.
    ReadClipboard,
    /// Look for an app update (the user asked).
    CheckUpdate,
    /// The periodic update check (at most every 6 hours).
    BackgroundCheck,
    /// Download, verify and stage the offered update (the user accepted).
    ApplyUpdate,
    /// Quit and start again now, installing a staged update first.
    RestartApp,
    /// The shell's platform support rows.
    PlatformSupport,
    /// The `[desktop]` options read at launch, with their notices.
    LaunchOptions,
    /// The deep links that arrived before the window was ready; later
    /// ones come as `deep-link` events.
    TakeLinks,
    /// An approval for an unfocused window: post the OS notification.
    NotifyApproval(crate::attention::Approval),
    /// A request was answered or settled: its notification opens nothing.
    RequestSettled {
        /// The server request id.
        request_id: String,
    },
    /// What the tray menu lists.
    SetTray(crate::attention::TrayState),
    /// The quick-ask session to reuse for `profile`, if any.
    QuickAskTarget {
        /// The profile asked.
        profile: String,
    },
    /// A question went to this quick-ask session.
    QuickAskUsed {
        /// The profile.
        profile: String,
        /// The session.
        session_id: String,
    },
    /// Open a session in the main window (from the quick-ask panel).
    OpenInMain {
        /// The profile.
        profile: String,
        /// The session.
        session_id: String,
    },
    /// Hide the calling panel window.
    HidePanel,
    /// The profile's plugin panels.
    PluginPanels {
        /// The profile.
        profile: String,
    },
    /// The install dialog's facts about a catalog entry.
    CatalogEntry {
        /// `plugin` or `skill`.
        kind: crate::catalog::Kind,
        /// The catalog id.
        id: String,
        /// The profile it would be installed into.
        profile: String,
    },
    /// Install a catalog entry (after the user confirmed in the dialog).
    CatalogInstall {
        /// `plugin` or `skill`.
        kind: crate::catalog::Kind,
        /// The catalog id.
        id: String,
        /// The profile.
        profile: String,
    },
    /// A plugin frame asked for a native command: refused and logged.
    PluginRefused {
        /// The plugin id.
        plugin: String,
        /// What it asked for.
        command: String,
    },
}

/// The machine integration behind the allowlist.
pub trait Native: Send + Sync {
    /// Opens an `http:`, `https:` or `mailto:` URL in the OS handler.
    ///
    /// # Errors
    ///
    /// Why the OS refused.
    fn open_url(&self, url: &str) -> Result<(), String>;
    /// Opens a checked local file with its default application.
    ///
    /// # Errors
    ///
    /// Why the OS refused.
    fn open_path(&self, path: &Path) -> Result<(), String>;
    /// Shows a checked file in the OS file manager.
    ///
    /// # Errors
    ///
    /// Why the OS refused.
    fn reveal(&self, path: &Path) -> Result<(), String>;
    /// Runs the OS picker (blocking); `None` when cancelled.
    fn pick(&self, directory: bool, title: Option<&str>) -> Option<PathBuf>;
    /// Posts a notification.
    ///
    /// # Errors
    ///
    /// Why it could not be posted.
    fn notify(&self, title: &str, body: &str) -> Result<(), String>;
    /// Sets or clears the badge of `window`.
    ///
    /// # Errors
    ///
    /// Why it could not be set.
    fn set_badge(&self, window: &str, count: Option<u32>) -> Result<(), String>;
    /// Registers a global shortcut.
    ///
    /// # Errors
    ///
    /// Why not (another application owns it, say).
    fn register_hotkey(&self, chord: &str) -> Result<(), String>;
    /// The clipboard's text.
    ///
    /// # Errors
    ///
    /// Why it could not be read.
    fn read_clipboard(&self) -> Result<String, String>;
    /// Shows `menu` in the tray.
    ///
    /// # Errors
    ///
    /// Why the tray could not be updated.
    fn set_tray(&self, menu: &[crate::attention::MenuEntry]) -> Result<(), String> {
        let _ = menu;
        Ok(())
    }
    /// Shows the main window with `session_id` of `profile` open.
    ///
    /// # Errors
    ///
    /// No main window.
    fn open_in_main(&self, profile: &str, session_id: &str) -> Result<(), String> {
        let _ = (profile, session_id);
        Err("no main window".to_owned())
    }
    /// Hides the panel window `window`.
    ///
    /// # Errors
    ///
    /// No such window.
    fn hide_window(&self, window: &str) -> Result<(), String> {
        let _ = window;
        Ok(())
    }
    /// Installs a staged update and starts the app again (the daemon is
    /// not stopped: the new app restarts it when it finds it stale).
    ///
    /// # Errors
    ///
    /// When the app cannot restart itself.
    fn restart(&self) -> Result<(), String> {
        Err("this shell cannot restart itself".to_owned())
    }
    /// What the platform probes look at.
    fn facts(&self) -> crate::platform::Facts {
        crate::platform::Facts::detect(false)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::Request;

    #[test]
    fn requests_are_typed_and_anything_else_is_rejected() {
        let open: Request = serde_json::from_value(
            json!({"command": "open_external", "args": {"url": "https://x"}}),
        )
        .unwrap();
        assert_eq!(
            open,
            Request::OpenExternal {
                url: "https://x".into(),
                workspace: None
            }
        );
        let unit: Request = serde_json::from_value(json!({"command": "read_clipboard"})).unwrap();
        assert_eq!(unit, Request::ReadClipboard);
        for bad in [
            json!({"command": "run_shell", "args": {"cmd": "ls"}}),
            json!({"command": "open_external", "args": {"url": 1}}),
            json!({"command": "open_external", "args": {"url": "https://x", "extra": true}}),
            json!({"command": "open_external", "args": {"url": "x"}, "more": 1}),
        ] {
            assert!(
                serde_json::from_value::<Request>(bad.clone()).is_err(),
                "{bad}"
            );
        }
    }
}
