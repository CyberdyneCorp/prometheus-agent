//! The user's opt-outs that hide desktop UI tools (spec: desktop-app,
//! Desktop UI toolset contract; Agent reactions; Pane focus, tours and
//! tips): read from the machine's UI preferences when a session's tools
//! are selected, so a change applies from the next session on.
//!
//! The preferences file is the daemon's `<home>/ui-prefs.json` (written
//! by `ui.prefs.patch`); a missing or unreadable file means the defaults:
//! reactions off, tours and tips on.

use std::path::Path;

use serde_json::Value;

/// `ui.prefs` key: the agent may react to messages (default off).
pub const REACTIONS: &str = "message_reactions";
/// `ui.prefs` key: guided tours (default on).
pub const TOURS: &str = "tours_enabled";
/// `ui.prefs` key: in-app tips (default on).
pub const TIPS: &str = "tips_enabled";

/// The desktop tools the preferences under `home` hide.
#[must_use]
pub fn hidden_tools(home: &Path) -> Vec<&'static str> {
    let prefs = std::fs::read(home.join("ui-prefs.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|v| v.get("prefs").cloned())
        .unwrap_or(Value::Null);
    let flag =
        |key: &str, default: bool| prefs.get(key).and_then(Value::as_bool).unwrap_or(default);
    let mut hidden = Vec::new();
    if !flag(REACTIONS, false) {
        hidden.push("react_to_message");
    }
    if !flag(TOURS, true) {
        hidden.push("gui_tour");
    }
    if !flag(TIPS, true) {
        hidden.push("show_tip");
    }
    hidden
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_hide_reactions_only_and_flags_toggle_each_tool() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(hidden_tools(home.path()), ["react_to_message"]);
        std::fs::write(
            home.path().join("ui-prefs.json"),
            r#"{"prefs": {"message_reactions": true, "tours_enabled": false, "tips_enabled": false}, "revision": 3}"#,
        )
        .unwrap();
        assert_eq!(hidden_tools(home.path()), ["gui_tour", "show_tip"]);
    }
}
