//! UI string catalogs (spec: i18n).
//!
//! Static UI text (CLI prompts, approval prompts, gateway notices, TUI
//! cards) is looked up by dotted key in layered catalogs selected by
//! `display.language`: enabled language packs, then the profile overlay
//! `<profile_home>/locales/<lang>[.tui|.desktop].toml`, then the bundled
//! catalog, then the same three layers for English, then the raw key.
//! Every layer may be partial.
//!
//! Localized text is UI only: it never enters a model request, so changing
//! the language can never break a session's prompt cache.

mod bundled;
pub mod catalog;
mod languages;
mod strings;
pub mod validate;

pub use bundled::{BUNDLED, Bundled};
pub use languages::{Language, Origin, check_available, check_id, languages};
pub use strings::{PackLocale, Sources, Strings};

/// The default language and the source of truth for keys and placeholders.
pub const ENGLISH: &str = "en";

/// The overlay directory inside a profile home.
pub const OVERLAY_DIR: &str = "locales";

/// A catalog surface: which UI a catalog file localizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Surface {
    /// CLI, gateway and shared strings, with named `{placeholders}`.
    Core,
    /// The terminal UI, with positional `{0}` placeholders.
    Tui,
    /// The desktop app and dashboard, with positional placeholders.
    Desktop,
}

impl Surface {
    /// Every surface.
    pub const ALL: [Self; 3] = [Self::Core, Self::Tui, Self::Desktop];

    /// `core`, `tui` or `desktop`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Tui => "tui",
            Self::Desktop => "desktop",
        }
    }

    /// The catalog file name of language `id` for this surface:
    /// `<id>.toml`, `<id>.tui.toml` or `<id>.desktop.toml`.
    #[must_use]
    pub fn file_name(self, id: &str) -> String {
        match self {
            Self::Core => format!("{id}.toml"),
            Self::Tui => format!("{id}.tui.toml"),
            Self::Desktop => format!("{id}.desktop.toml"),
        }
    }

    /// The language id and surface a catalog file name stands for.
    #[must_use]
    pub fn of_file(name: &str) -> Option<(&str, Self)> {
        let stem = name.strip_suffix(".toml")?;
        let (id, surface) = if let Some(id) = stem.strip_suffix(".tui") {
            (id, Self::Tui)
        } else if let Some(id) = stem.strip_suffix(".desktop") {
            (id, Self::Desktop)
        } else {
            (stem, Self::Core)
        };
        check_id(id).ok().map(|()| (id, surface))
    }

    /// The English key set of this surface, sorted (what
    /// `schema/locales/keys.<surface>.json` holds).
    #[must_use]
    pub fn english_keys(self) -> Vec<String> {
        bundled::english(self).keys().cloned().collect()
    }
}
