//! The `[desktop]` section (spec: desktop-app, Desktop launch options;
//! Closing windows does not stop the agent; Global hotkey quick-ask).
//!
//! The desktop shell reads it before its first window opens. The launch
//! options are strings on purpose: an invalid value must not make the
//! profile invalid (the daemon would refuse to load it); the shell shows a
//! notice naming the key and its allowed values and uses the default.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[desktop]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "restart"))]
pub struct DesktopConfig {
    /// Keep the app in the tray (macOS menu bar, Windows notification
    /// area, Linux `StatusNotifierItem`) after its last window closes.
    pub keep_in_tray: bool,
    /// The global quick-ask hotkey.
    pub quick_ask: QuickAskConfig,
    /// `auto` (disable GPU acceleration only on remote displays), `on` or
    /// `off`.
    pub disable_gpu: String,
    /// `auto`, `wayland` or `x11` (X11 runs through `XWayland` on a Wayland
    /// session).
    pub linux_window_backend: String,
    /// `auto`, `secret-service` or `kwallet`.
    pub linux_secret_store: String,
    /// Expose the webview's accessibility tree to assistive technology.
    pub accessibility_tree: bool,
    /// Linux: keep the XDG `.desktop` launcher entry current; `false`
    /// preserves an existing entry and still creates a missing one.
    pub manage_launcher_entry: bool,
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            keep_in_tray: true,
            quick_ask: QuickAskConfig::default(),
            disable_gpu: "auto".to_owned(),
            linux_window_backend: "auto".to_owned(),
            linux_secret_store: "auto".to_owned(),
            accessibility_tree: true,
            manage_launcher_entry: true,
        }
    }
}

/// The `[desktop.quick_ask]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "restart"))]
pub struct QuickAskConfig {
    /// The global chord that opens the quick-ask panel; empty disables it.
    pub hotkey: String,
    /// The profile asked; empty is the last used profile.
    pub profile: String,
    /// A quick-ask session active within this many seconds is asked again
    /// instead of starting a new one.
    pub reuse_secs: u64,
}

impl Default for QuickAskConfig {
    fn default() -> Self {
        Self {
            hotkey: "CommandOrControl+Shift+Space".to_owned(),
            profile: String::new(),
            reuse_secs: 900,
        }
    }
}
