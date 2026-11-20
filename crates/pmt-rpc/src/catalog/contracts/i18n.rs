//! The language switchers' listing (spec: i18n, Language switchers show
//! endonyms): every language a profile can select, by its own name, so
//! the TUI picker, the desktop settings and the dashboard list the same
//! set and mirror their layout for a right-to-left one. The choice is
//! written with `config.patch` (`display.language`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Method, ProfileParams};

/// One selectable language.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LanguageEntry {
    /// The `display.language` id (`de`, `zh-hant`).
    pub id: String,
    /// The language's own name for itself (`Deutsch`).
    pub endonym: String,
    /// Written right to left: surfaces mirror their layout.
    pub rtl: bool,
    /// `bundled`, `overlay` or `pack:<plugin>`.
    pub origin: String,
}

/// `i18n.languages` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Languages {
    /// The profile's `display.language`.
    pub current: String,
    /// Every selectable language, by id.
    pub languages: Vec<LanguageEntry>,
}

method!(/// `i18n.languages`: the languages the profile can select (bundled,
    /// overlay and enabled packs) with their endonyms and direction.
    I18nLanguages = "i18n.languages" (ProfileParams) -> Languages);
