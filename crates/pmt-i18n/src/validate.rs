//! Language pack validation (`prometheus plugins validate`): every
//! declared language has its core catalog, every catalog parses with text
//! values only and English placeholders (errors), and keys English does
//! not have are listed as warnings (a typo, or a key of a newer release).

use std::path::Path;

use crate::catalog::{self, placeholders};
use crate::{OVERLAY_DIR, Surface, bundled};

/// What validation found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Problems that make the pack invalid.
    pub errors: Vec<String>,
    /// Problems worth fixing that do not.
    pub warnings: Vec<String>,
}

/// Checks the catalogs of `ids` under `<plugin_dir>/locales/`.
#[must_use]
pub fn pack(plugin_dir: &Path, ids: &[String]) -> Report {
    let mut report = Report::default();
    let dir = plugin_dir.join(OVERLAY_DIR);
    for id in ids {
        if let Err(err) = crate::check_id(id) {
            report.errors.push(err.message().to_owned());
            continue;
        }
        for surface in Surface::ALL {
            let name = surface.file_name(id);
            match std::fs::read_to_string(dir.join(&name)) {
                Ok(text) => file(
                    &format!("{OVERLAY_DIR}/{name}"),
                    &text,
                    surface,
                    &mut report,
                ),
                Err(_) if surface == Surface::Core => report.errors.push(format!(
                    "{OVERLAY_DIR}/{name} is missing (language '{id}' is declared)"
                )),
                Err(_) => {}
            }
        }
    }
    report
}

/// Checks one catalog's text against the English catalog of `surface`.
pub fn file(name: &str, text: &str, surface: Surface, report: &mut Report) {
    let entries = match catalog::parse(text) {
        Ok(entries) => entries,
        Err(reason) => {
            report.errors.push(format!("{name}: {reason}"));
            return;
        }
    };
    let english = bundled::english(surface);
    for (key, value) in &entries {
        match english.get(key) {
            None => report.warnings.push(format!("{name}: unknown key {key}")),
            Some(reference) if placeholders(value) != placeholders(reference) => {
                report.errors.push(format!(
                    "{name}: {key} uses placeholders {} but English uses {}",
                    braces(value),
                    braces(reference)
                ));
            }
            Some(_) => {}
        }
    }
}

fn braces(text: &str) -> String {
    let names: Vec<String> = placeholders(text)
        .iter()
        .map(|p| format!("{{{p}}}"))
        .collect();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("locales")).unwrap();
        for (name, text) in files {
            std::fs::write(dir.path().join("locales").join(name), text).unwrap();
        }
        dir
    }

    /// Scenario: Unknown key warned not failed.
    #[test]
    fn an_unknown_key_is_a_warning() {
        let dir = pack_dir(&[("pl.toml", "[approval]\ndenyed = \"Odrzucono\"\n")]);
        let report = pack(dir.path(), &["pl".into()]);
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(
            report.warnings,
            ["locales/pl.toml: unknown key approval.denyed"]
        );
    }

    #[test]
    fn a_missing_core_catalog_a_non_text_value_and_a_misfit_placeholder_are_errors() {
        let dir = pack_dir(&[
            ("pl.tui.toml", "[tui]\nanswer_within = \"({1} s)\"\n"),
            ("de.toml", "[approval]\ndenied = [\"x\"]\n"),
        ]);
        let report = pack(dir.path(), &["pl".into(), "de".into()]);
        assert_eq!(
            report.errors,
            [
                "locales/pl.toml is missing (language 'pl' is declared)",
                "locales/pl.tui.toml: tui.answer_within uses placeholders {1} but English uses {0}",
                "locales/de.toml: approval.denied: value is not text",
            ]
        );
    }
}
