//! `[dashboard]` of `plugin.toml` (spec: web-dashboard, Dashboard plugin
//! contributions): what a plugin adds to the web dashboard.
//!
//! ```toml
//! [dashboard]
//! label = "Weather"
//! icon = "cloud"                    # optional
//! entry = "dashboard/index.html"    # optional, the default
//! slots = ["header-right", "sessions:top"]
//!
//! [dashboard.tab]
//! path = "/weather"
//! position = "after:sessions"       # end | after:<page> | before:<page>
//! override = "/sessions"            # optional: replaces one built-in page
//! hidden = false                    # a routable page with no nav entry
//! ```
//!
//! Its page and slots render the `entry` HTML file in sandboxed frames; a
//! plugin that also implements the `ui-panel` world serves backend routes
//! under `/api/plugins/<id>/`.

use toml::{Table, Value};

use super::invalid;
use pmt_core::error::Error;

/// The dashboard's built-in pages: what `position` and `override` may
/// name, and the pages that have `<page>:top|bottom` slots.
pub const PAGES: &[&str] = &[
    "chat",
    "sessions",
    "usage",
    "models",
    "skills",
    "toolsets",
    "plugins",
    "mcp",
    "cron",
    "adapters",
    "pairing",
    "webhooks",
    "memory",
    "logs",
    "status",
    "profiles",
    "config",
    "secrets",
    "kanban",
    "evolution",
    "experience",
    "files",
    "docs",
    "profile-builder",
    "themes",
    "achievements",
];

/// The shell slots (`sidebar` exists in the cockpit layout only).
pub const SHELL_SLOTS: &[&str] = &[
    "header-left",
    "header-right",
    "header-banner",
    "sidebar",
    "pre-main",
    "post-main",
    "footer-left",
    "footer-right",
    "overlay",
];

/// The default `entry`.
pub const DEFAULT_ENTRY: &str = "dashboard/index.html";

const FIELDS: &[&str] = &["label", "icon", "entry", "slots", "tab"];
const TAB_FIELDS: &[&str] = &["path", "position", "override", "hidden"];

/// Where a tab goes in the navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Position {
    /// After every built-in page.
    End,
    /// Right after a built-in page.
    After(String),
    /// Right before a built-in page.
    Before(String),
}

impl Position {
    /// The manifest spelling.
    #[must_use]
    pub fn as_text(&self) -> String {
        match self {
            Self::End => "end".to_owned(),
            Self::After(page) => format!("after:{page}"),
            Self::Before(page) => format!("before:{page}"),
        }
    }
}

/// `[dashboard.tab]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    /// The route of the plugin's page (`/weather`).
    pub path: String,
    /// Where its navigation entry goes.
    pub position: Position,
    /// The built-in route it replaces (`/sessions`).
    pub override_route: Option<String>,
    /// Routable, but without a navigation entry.
    pub hidden: bool,
}

/// `[dashboard]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardDeclaration {
    /// The tab's and frames' label.
    pub label: String,
    /// An icon name.
    pub icon: Option<String>,
    /// The HTML file its frames load, relative to the plugin directory.
    pub entry: String,
    /// Its page, when it has one.
    pub tab: Option<Tab>,
    /// The slots it fills.
    pub slots: Vec<String>,
}

fn text(table: &Table, key: &str, at: &str) -> Result<Option<String>, Error> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.clone())),
        Some(_) => Err(invalid(format!("{at}.{key} must be a non-empty string"))),
    }
}

fn unknown(table: &Table, allowed: &[&str], at: &str) -> Result<(), Error> {
    match table.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(key) => Err(invalid(format!("unknown manifest field '{at}.{key}'"))),
        None => Ok(()),
    }
}

/// `/<segment>`: one lower-case route segment.
fn check_route(route: &str, key: &str) -> Result<(), Error> {
    let segment = route.strip_prefix('/').unwrap_or_default();
    let valid = !segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
        Ok(())
    } else {
        Err(invalid(format!(
            "dashboard.tab.{key} '{route}' must be one route segment like /weather"
        )))
    }
}

fn page(name: &str, key: &str) -> Result<String, Error> {
    if PAGES.contains(&name) {
        Ok(name.to_owned())
    } else {
        Err(invalid(format!(
            "dashboard.tab.{key} names no built-in page '{name}'"
        )))
    }
}

fn position(text: Option<String>) -> Result<Position, Error> {
    let Some(text) = text else {
        return Ok(Position::End);
    };
    if text == "end" {
        return Ok(Position::End);
    }
    match text.split_once(':') {
        Some(("after", name)) => Ok(Position::After(page(name, "position")?)),
        Some(("before", name)) => Ok(Position::Before(page(name, "position")?)),
        _ => Err(invalid(format!(
            "dashboard.tab.position '{text}' is not end, after:<page> or before:<page>"
        ))),
    }
}

fn tab(value: &Value) -> Result<Tab, Error> {
    let Value::Table(table) = value else {
        return Err(invalid("dashboard.tab must be a table".to_owned()));
    };
    unknown(table, TAB_FIELDS, "dashboard.tab")?;
    let path = text(table, "path", "dashboard.tab")?
        .ok_or_else(|| invalid("missing manifest field 'dashboard.tab.path'".to_owned()))?;
    check_route(&path, "path")?;
    let override_route = text(table, "override", "dashboard.tab")?;
    if let Some(route) = &override_route {
        check_route(route, "override")?;
        page(route.trim_start_matches('/'), "override")?;
    }
    let hidden = match table.get("hidden") {
        None => false,
        Some(Value::Boolean(b)) => *b,
        Some(_) => return Err(invalid("dashboard.tab.hidden must be a boolean".to_owned())),
    };
    Ok(Tab {
        position: position(text(table, "position", "dashboard.tab")?)?,
        path,
        override_route,
        hidden,
    })
}

fn slot(name: &str) -> bool {
    SHELL_SLOTS.contains(&name)
        || name
            .split_once(':')
            .is_some_and(|(page, place)| PAGES.contains(&page) && matches!(place, "top" | "bottom"))
}

/// A relative path inside the plugin directory.
fn check_entry(entry: &str) -> Result<(), Error> {
    let path = std::path::Path::new(entry);
    let inside = path
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    let html = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("html"));
    if inside && html {
        Ok(())
    } else {
        Err(invalid(format!(
            "dashboard.entry '{entry}' must be an .html file inside the plugin directory"
        )))
    }
}

/// Parses `[dashboard]`.
///
/// # Errors
///
/// `plugins.manifest` naming the field.
pub(crate) fn parse(value: Option<&Value>) -> Result<Option<DashboardDeclaration>, Error> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Value::Table(table) = value else {
        return Err(invalid("dashboard must be a table".to_owned()));
    };
    unknown(table, FIELDS, "dashboard")?;
    let label = text(table, "label", "dashboard")?
        .ok_or_else(|| invalid("missing manifest field 'dashboard.label'".to_owned()))?;
    let entry = text(table, "entry", "dashboard")?.unwrap_or_else(|| DEFAULT_ENTRY.to_owned());
    check_entry(&entry)?;
    let slots = super::strings(table, "slots")
        .map_err(|_| invalid("dashboard.slots must be a list of strings".to_owned()))?;
    if let Some(bad) = slots.iter().find(|s| !slot(s)) {
        return Err(invalid(format!("dashboard.slots: unknown slot '{bad}'")));
    }
    Ok(Some(DashboardDeclaration {
        label,
        icon: text(table, "icon", "dashboard")?,
        entry,
        tab: table.get("tab").map(tab).transpose()?,
        slots,
    }))
}

#[cfg(test)]
mod tests {
    use super::{Position, parse};

    fn parsed(text: &str) -> Result<Option<super::DashboardDeclaration>, String> {
        let table: toml::Table = text.parse().unwrap();
        parse(table.get("dashboard")).map_err(|e| e.message().to_owned())
    }

    #[test]
    fn a_full_declaration_parses() {
        let declared = parsed(
            "[dashboard]\nlabel = \"Weather\"\nicon = \"cloud\"\nslots = [\"header-right\", \"sessions:top\"]\n\
             [dashboard.tab]\npath = \"/weather\"\nposition = \"after:sessions\"\noverride = \"/sessions\"\nhidden = true\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(declared.entry, "dashboard/index.html");
        let tab = declared.tab.unwrap();
        assert_eq!(tab.position, Position::After("sessions".into()));
        assert_eq!(tab.override_route.as_deref(), Some("/sessions"));
        assert!(tab.hidden);
        assert_eq!(declared.slots, ["header-right", "sessions:top"]);
    }

    #[test]
    fn bad_declarations_name_the_field() {
        for (text, wanted) in [
            ("[dashboard]\nicon = \"x\"\n", "dashboard.label"),
            ("[dashboard]\nlabel = \"x\"\ncolor = 1\n", "dashboard.color"),
            (
                "[dashboard]\nlabel = \"x\"\nslots = [\"nowhere\"]\n",
                "unknown slot 'nowhere'",
            ),
            (
                "[dashboard]\nlabel = \"x\"\nentry = \"../x.html\"\n",
                "dashboard.entry",
            ),
            (
                "[dashboard]\nlabel = \"x\"\n[dashboard.tab]\npath = \"weather\"\n",
                "dashboard.tab.path",
            ),
            (
                "[dashboard]\nlabel = \"x\"\n[dashboard.tab]\npath = \"/w\"\nposition = \"middle\"\n",
                "dashboard.tab.position",
            ),
            (
                "[dashboard]\nlabel = \"x\"\n[dashboard.tab]\npath = \"/w\"\noverride = \"/nowhere\"\n",
                "dashboard.tab.override",
            ),
        ] {
            let err = parsed(text).unwrap_err();
            assert!(err.contains(wanted), "{text}: {err}");
        }
        assert_eq!(parsed("x = 1\n").unwrap(), None);
    }
}
