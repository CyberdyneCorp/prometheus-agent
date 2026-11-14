//! `[ui]` of `plugin.toml` (spec: desktop-app, Plugin UI contributions are
//! isolated): a panel the desktop app loads in a sandboxed frame on the
//! plugin's own origin.
//!
//! ```toml
//! [capabilities]
//! ui = true
//!
//! [ui]
//! label = "Weather"
//! entry = "ui/index.html"     # optional, the default
//! sdk = ["sessions.read"]     # what the frame may ask the app for
//! ```
//!
//! A panel talks to the app only through the plugin UI SDK message
//! channel, and only for the `sdk` capabilities its manifest lists;
//! approving the plugin's `ui` capability approves those (the approval
//! shows them).

use toml::Value;

use super::invalid;
use pmt_core::error::Error;

/// The default `entry`.
pub const DEFAULT_ENTRY: &str = "ui/index.html";
/// The capabilities the plugin UI SDK offers a panel.
pub const SDK_CAPABILITIES: &[&str] = &["sessions.read", "connections.read", "agents.open"];

const FIELDS: &[&str] = &["label", "entry", "sdk"];

/// `[ui]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiDeclaration {
    /// The panel's label.
    pub label: String,
    /// The HTML file the frame loads, relative to the plugin directory.
    pub entry: String,
    /// The SDK capabilities it asks for.
    pub sdk: Vec<String>,
}

/// Parses `[ui]`; `ui_granted` is `capabilities.ui`.
///
/// # Errors
///
/// `plugins.manifest` naming the field.
pub(crate) fn parse(
    value: Option<&Value>,
    ui_capability: bool,
) -> Result<Option<UiDeclaration>, Error> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Value::Table(table) = value else {
        return Err(invalid("ui must be a table".to_owned()));
    };
    if let Some(key) = table.keys().find(|k| !FIELDS.contains(&k.as_str())) {
        return Err(invalid(format!("unknown manifest field 'ui.{key}'")));
    }
    if !ui_capability {
        return Err(invalid("[ui] needs capabilities.ui = true".to_owned()));
    }
    let label = match table.get("label") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
        Some(_) => return Err(invalid("ui.label must be a non-empty string".to_owned())),
        None => return Err(invalid("missing manifest field 'ui.label'".to_owned())),
    };
    let entry = match table.get("entry") {
        None => DEFAULT_ENTRY.to_owned(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => return Err(invalid("ui.entry must be a string".to_owned())),
    };
    check_entry(&entry)?;
    let sdk = super::strings(table, "sdk")
        .map_err(|_| invalid("ui.sdk must be a list of strings".to_owned()))?;
    if let Some(bad) = sdk.iter().find(|c| !SDK_CAPABILITIES.contains(&c.as_str())) {
        return Err(invalid(format!("ui.sdk: unknown SDK capability '{bad}'")));
    }
    Ok(Some(UiDeclaration { label, entry, sdk }))
}

fn check_entry(entry: &str) -> Result<(), Error> {
    let path = std::path::Path::new(entry);
    let inside = path
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    if inside
        && path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("html"))
    {
        Ok(())
    } else {
        Err(invalid(format!(
            "ui.entry '{entry}' must be an .html file inside the plugin directory"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    fn parsed(text: &str, ui: bool) -> Result<Option<super::UiDeclaration>, String> {
        let table: toml::Table = text.parse().unwrap();
        parse(table.get("ui"), ui).map_err(|e| e.message().to_owned())
    }

    #[test]
    fn a_panel_declares_its_label_entry_and_sdk_capabilities() {
        let ui = parsed("[ui]\nlabel = \"A\"\nsdk = [\"sessions.read\"]\n", true)
            .unwrap()
            .unwrap();
        assert_eq!(ui.entry, "ui/index.html");
        assert_eq!(ui.sdk, ["sessions.read"]);
        assert_eq!(parsed("", true), Ok(None));
    }

    #[test]
    fn unknown_capabilities_paths_and_ungranted_ui_are_refused() {
        assert!(
            parsed("[ui]\nlabel = \"A\"\nsdk = [\"fs.write\"]\n", true)
                .unwrap_err()
                .contains("unknown SDK capability")
        );
        assert!(parsed("[ui]\nlabel = \"A\"\nentry = \"../x.html\"\n", true).is_err());
        assert!(
            parsed("[ui]\nlabel = \"A\"\n", false)
                .unwrap_err()
                .contains("capabilities.ui")
        );
    }
}
