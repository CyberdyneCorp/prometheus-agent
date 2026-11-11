//! The catalogs compiled into the binary: complete `core` and `tui`
//! catalogs for every bundled language, English being the source of truth.

use std::sync::OnceLock;

use crate::Surface;
use crate::catalog::{self, Entries};

/// One bundled language.
#[derive(Debug, Clone, Copy)]
pub struct Bundled {
    /// The `display.language` id.
    pub id: &'static str,
    /// Its own name for itself, shown by language pickers.
    pub endonym: &'static str,
    /// Written right to left.
    pub rtl: bool,
    /// `<id>.toml`.
    core: &'static str,
    /// `<id>.tui.toml`.
    tui: &'static str,
}

macro_rules! bundled {
    ($($id:literal, $endonym:literal, $rtl:literal;)*) => {
        /// Every bundled language, by id.
        pub const BUNDLED: &[Bundled] = &[$(Bundled {
            id: $id,
            endonym: $endonym,
            rtl: $rtl,
            core: include_str!(concat!("../locales/", $id, ".toml")),
            tui: include_str!(concat!("../locales/", $id, ".tui.toml")),
        },)*];
    };
}

bundled! {
    "af", "Afrikaans", false;
    "ar", "العربية", true;
    "de", "Deutsch", false;
    "en", "English", false;
    "es", "Español", false;
    "fr", "Français", false;
    "ga", "Gaeilge", false;
    "hu", "Magyar", false;
    "it", "Italiano", false;
    "ja", "日本語", false;
    "ko", "한국어", false;
    "pt", "Português", false;
    "ru", "Русский", false;
    "tr", "Türkçe", false;
    "uk", "Українська", false;
    "zh", "简体中文", false;
    "zh-hant", "繁體中文", false;
}

impl Bundled {
    /// The bundled language `id`.
    #[must_use]
    pub fn get(id: &str) -> Option<&'static Self> {
        BUNDLED.iter().find(|b| b.id == id)
    }

    /// The catalog text of `surface` (empty: none bundled; the desktop
    /// strings ship with the desktop app).
    #[must_use]
    pub fn text(&self, surface: Surface) -> &'static str {
        match surface {
            Surface::Core => self.core,
            Surface::Tui => self.tui,
            Surface::Desktop => "",
        }
    }
}

/// The parsed bundled catalog of `id` for `surface` (empty when not
/// bundled). Bundled catalogs are checked by this crate's tests, so a
/// parse failure cannot ship; it would read as empty.
pub(crate) fn entries(id: &str, surface: Surface) -> &'static Entries {
    static PARSED: OnceLock<Vec<[Entries; 3]>> = OnceLock::new();
    static EMPTY: Entries = Entries::new();
    let parsed = PARSED.get_or_init(|| {
        BUNDLED
            .iter()
            .map(|b| Surface::ALL.map(|s| catalog::parse(b.text(s)).unwrap_or_default()))
            .collect()
    });
    let slot = Surface::ALL.iter().position(|s| *s == surface).unwrap_or(0);
    BUNDLED
        .iter()
        .position(|b| b.id == id)
        .map_or(&EMPTY, |index| &parsed[index][slot])
}

/// The English bundled catalog of `surface`.
pub(crate) fn english(surface: Surface) -> &'static Entries {
    entries(crate::ENGLISH, surface)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::placeholders;

    /// Scenario: the CI gate. Every bundled catalog parses, holds exactly
    /// the English keys and uses the English placeholders.
    #[test]
    fn every_bundled_catalog_is_complete_with_english_placeholders() {
        assert_eq!(BUNDLED.len(), 17);
        for surface in [Surface::Core, Surface::Tui] {
            let english = catalog::parse(Bundled::get("en").unwrap().text(surface)).unwrap();
            assert!(!english.is_empty());
            for bundled in BUNDLED {
                let entries = catalog::parse(bundled.text(surface))
                    .unwrap_or_else(|e| panic!("{} {}: {e}", bundled.id, surface.as_str()));
                let keys: Vec<_> = entries.keys().collect();
                let expected: Vec<_> = english.keys().collect();
                assert_eq!(keys, expected, "{} {} keys", bundled.id, surface.as_str());
                for (key, text) in &entries {
                    assert_eq!(
                        placeholders(text),
                        placeholders(&english[key]),
                        "{} {} {key}",
                        bundled.id,
                        surface.as_str()
                    );
                }
            }
        }
    }

    #[test]
    fn only_arabic_is_right_to_left() {
        let rtl: Vec<_> = BUNDLED.iter().filter(|b| b.rtl).map(|b| b.id).collect();
        assert_eq!(rtl, ["ar"]);
    }
}
