//! The install consent flow's checks (spec: desktop-app, Install deep
//! links open a consent flow). A `prometheus://plugins|skills/install`
//! link names a catalog id; the shell refuses anything else before it
//! runs or downloads anything, then asks the bundled binary what the
//! configured catalog says about the id (no download of the entry), and
//! installs only when the window says the user confirmed in the dialog.

use serde::{Deserialize, Serialize};

/// What a link installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A plugin catalog entry.
    Plugin,
    /// An official catalog skill (`official/<category>/<name>`).
    Skill,
}

impl Kind {
    /// The bundled binary's spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plugin => "plugin",
            Self::Skill => "skill",
        }
    }
}

/// The refusal of anything that is not a catalog entry.
#[must_use]
pub fn unknown(id: &str) -> String {
    format!("Unknown catalog entry {id}")
}

fn segment(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && text.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
}

/// Whether `id` is shaped like an id of `kind`'s catalog: a plugin id
/// (`kanban-extras`) or `official/<category>/<name>`. URLs, paths and
/// anything else are not.
#[must_use]
pub fn is_catalog_id(kind: Kind, id: &str) -> bool {
    match kind {
        Kind::Plugin => segment(id),
        Kind::Skill => id
            .strip_prefix("official/")
            .is_some_and(|rest| matches!(rest.split('/').collect::<Vec<_>>()[..], [c, n] if segment(c) && segment(n))),
    }
}

/// What the dialog shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// `plugin` or `skill`.
    pub kind: Kind,
    /// The catalog id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// One line.
    #[serde(default)]
    pub description: String,
    /// Who publishes it.
    pub publisher: String,
    /// The version installed.
    pub version: String,
    /// Its signature status.
    pub signature: String,
    /// What it asks for.
    pub capabilities: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::{Kind, is_catalog_id};

    #[test]
    fn only_catalog_ids_pass() {
        assert!(is_catalog_id(Kind::Plugin, "kanban-extras"));
        assert!(is_catalog_id(Kind::Skill, "official/research/arxiv"));
        for bad in [
            "https://evil.example/p.wasm",
            "../../etc/passwd",
            "/tmp/p.pmtpkg",
            "Kanban",
            "",
        ] {
            assert!(!is_catalog_id(Kind::Plugin, bad), "{bad}");
        }
        assert!(!is_catalog_id(Kind::Skill, "official/x"));
        assert!(!is_catalog_id(Kind::Skill, "owner/repo/path"));
    }
}
