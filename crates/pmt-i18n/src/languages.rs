//! Which languages a profile can select: bundled ones, any language a
//! profile overlay supplies, and those of enabled language packs.

use std::collections::BTreeMap;
use std::path::Path;

use pmt_core::error::{Error, codes};

use crate::{BUNDLED, Sources, Surface};

/// Where a selectable language comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Compiled into the binary.
    Bundled,
    /// `<profile_home>/locales/<id>[.tui|.desktop].toml`.
    Overlay,
    /// An enabled language-pack plugin.
    Pack(String),
}

/// One selectable language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Language {
    /// The `display.language` id.
    pub id: String,
    /// Its own name for itself.
    pub endonym: String,
    /// Written right to left.
    pub rtl: bool,
    /// Where it comes from (the first of bundled, pack, overlay).
    pub origin: Origin,
}

/// Checks the id form `^[a-z]{2,3}(-[a-z0-9]{2,8})*$`.
///
/// # Errors
///
/// `config.invalid` naming the id.
pub fn check_id(id: &str) -> Result<(), Error> {
    let mut parts = id.split('-');
    let primary = parts.next().unwrap_or_default();
    let valid = (2..=3).contains(&primary.len())
        && primary.chars().all(|c| c.is_ascii_lowercase())
        && parts.all(|p| {
            (2..=8).contains(&p.len())
                && p.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        });
    if valid {
        Ok(())
    } else {
        Err(Error::new(
            &codes::CONFIG_INVALID,
            format!(
                "display.language: '{id}' is not a language id (lower case, like 'de', 'pt' or 'zh-hant')"
            ),
        ))
    }
}

/// Every language `sources` can render, by id.
#[must_use]
pub fn languages(sources: &Sources) -> Vec<Language> {
    let mut found: BTreeMap<String, Language> = BTreeMap::new();
    for bundled in BUNDLED {
        found.insert(
            bundled.id.to_owned(),
            Language {
                id: bundled.id.to_owned(),
                endonym: bundled.endonym.to_owned(),
                rtl: bundled.rtl,
                origin: Origin::Bundled,
            },
        );
    }
    for pack in &sources.packs {
        found.entry(pack.id.clone()).or_insert_with(|| Language {
            id: pack.id.clone(),
            endonym: pack.endonym.clone(),
            rtl: pack.rtl,
            origin: Origin::Pack(pack.plugin.clone()),
        });
    }
    if let Some(dir) = &sources.overlay_dir {
        for id in overlay_ids(dir) {
            found.entry(id.clone()).or_insert_with(|| Language {
                endonym: id.clone(),
                id,
                rtl: false,
                origin: Origin::Overlay,
            });
        }
    }
    found.into_values().collect()
}

/// The language ids of the catalog files in `dir`.
fn overlay_ids(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            Surface::of_file(&name).map(|(id, _)| id.to_owned())
        })
        .collect()
}

/// Refuses a language no layer of `sources` supplies, naming the
/// available ids and how to add one under `profile_dir`.
///
/// # Errors
///
/// `config.invalid` (exit 78): `Unknown language '<id>' for
/// display.language. Available: <ids>`.
pub fn check_available(id: &str, sources: &Sources, profile_dir: &Path) -> Result<(), Error> {
    check_id(id)?;
    let available = languages(sources);
    if available.iter().any(|l| l.id == id) {
        return Ok(());
    }
    let ids: Vec<&str> = available.iter().map(|l| l.id.as_str()).collect();
    Err(Error::new(
        &codes::CONFIG_INVALID,
        format!(
            "Unknown language '{id}' for display.language. Available: {}",
            ids.join(", ")
        ),
    )
    .with_hint(format!(
        "install a language pack plugin that provides '{id}', or add {}",
        profile_dir
            .join(crate::OVERLAY_DIR)
            .join(Surface::Core.file_name(id))
            .display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_lower_case_bcp47_style() {
        for good in ["en", "de", "zh-hant", "pt-br", "ast"] {
            assert!(check_id(good).is_ok(), "{good}");
        }
        for bad in ["", "e", "EN", "english", "zh_hant", "pt-", "de-x"] {
            assert!(check_id(bad).is_err(), "{bad}");
        }
    }

    /// Scenario: Bundled language list.
    #[test]
    fn an_unknown_id_names_every_bundled_language() {
        let dir = tempfile::tempdir().unwrap();
        let sources = Sources::of_profile("en", dir.path(), Vec::new());
        let err = check_available("xx", &sources, dir.path()).unwrap_err();
        assert_eq!(err.code(), &codes::CONFIG_INVALID);
        assert!(
            err.message()
                .starts_with("Unknown language 'xx' for display.language. Available: ")
        );
        for bundled in BUNDLED {
            assert!(err.message().contains(bundled.id), "{}", bundled.id);
        }
        assert!(err.hint().unwrap_or_default().contains("locales"));
    }

    #[test]
    fn an_overlay_makes_its_language_selectable() {
        let dir = tempfile::tempdir().unwrap();
        let sources = Sources::of_profile("en", dir.path(), Vec::new());
        assert!(check_available("pl", &sources, dir.path()).is_err());
        std::fs::create_dir_all(dir.path().join("locales")).unwrap();
        std::fs::write(dir.path().join("locales/pl.tui.toml"), "").unwrap();
        assert!(check_available("pl", &sources, dir.path()).is_ok());
        let pl = languages(&sources)
            .into_iter()
            .find(|l| l.id == "pl")
            .unwrap();
        assert_eq!(pl.origin, Origin::Overlay);
    }
}
