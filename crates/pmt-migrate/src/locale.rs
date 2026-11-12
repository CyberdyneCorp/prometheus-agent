//! The UI language of a Hermes profile (spec: i18n, Migration of Hermes
//! language settings), part of the `identity` component.
//!
//! `display.language` (or `HERMES_LANGUAGE` in the Hermes `.env`, which
//! wins as it does in Hermes) becomes `display.language`, never an
//! environment variable. Each profile overlay
//! `<hermes_home>/locales/<lang>[.tui|.desktop].yaml` becomes the TOML
//! overlay `<profile_home>/locales/<lang>[.tui|.desktop].toml`, keeping
//! text values only and reporting each skipped entry. Overlays are written
//! before the language, so a language only an overlay supplies can be
//! selected.

use std::path::PathBuf;

use pmt_core::error::{Error, codes};
use pmt_i18n::{Sources, Surface, catalog};
use serde_json::Value;

use crate::component::Component;
use crate::ledger;
use crate::plan::{Dest, Existing, Source};
use crate::report::{Disposition, Entry};
use crate::writes::{self, Writes};

/// The `.env` variable Hermes reads the language from.
pub(crate) const ENV_NAME: &str = "HERMES_LANGUAGE";

/// One overlay to write.
#[derive(Debug, Clone)]
struct Overlay {
    /// `locales/<file>.toml`, relative to the profile.
    target: String,
    text: String,
    entry: usize,
}

/// The planned language settings.
#[derive(Debug, Default)]
pub(crate) struct Locale {
    overlays: Vec<Overlay>,
    writes: Writes,
}

impl Locale {
    /// Plans the language and overlays of `src`, adding entries to
    /// `entries` (identity component indices).
    pub(crate) fn plan(src: &Source<'_>, entries: &mut Vec<Entry>) -> Self {
        let mut locale = Self::default();
        locale.plan_overlays(src, entries);
        locale.plan_language(src, entries);
        locale
    }

    fn plan_overlays(&mut self, src: &Source<'_>, entries: &mut Vec<Entry>) {
        let dir = src.profile.path(pmt_i18n::OVERLAY_DIR);
        let Ok(listing) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut files: Vec<(String, PathBuf)> = listing
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let stem = name
                    .strip_suffix(".yaml")
                    .or_else(|| name.strip_suffix(".yml"))?;
                let target = format!("{stem}.toml");
                Surface::of_file(&target)?;
                Some((format!("{}/{target}", pmt_i18n::OVERLAY_DIR), e.path()))
            })
            .collect();
        files.sort();
        for (target, path) in files {
            let item = format!(
                "locales/{}",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
            let entry = |d| Entry::new(Component::Identity, item.clone(), d);
            let parsed = std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|text| pmt_skills::yaml::parse(&text).map_err(|e| e.reason));
            let value = match parsed {
                Ok(value) => value,
                Err(why) => {
                    entries.push(entry(Disposition::Failed).because(why));
                    continue;
                }
            };
            let (converted, skipped) = catalog::from_json(&value);
            for key in skipped {
                entries.push(
                    Entry::new(
                        Component::Identity,
                        format!("{item}:{key}"),
                        Disposition::Skipped,
                    )
                    .because("not text"),
                );
            }
            self.overlays.push(Overlay {
                target: target.clone(),
                text: catalog::to_toml(&converted),
                entry: entries.len(),
            });
            entries.push(
                entry(Disposition::Transformed)
                    .to(&target)
                    .because("converted to TOML"),
            );
        }
    }

    fn plan_language(&mut self, src: &Source<'_>, entries: &mut Vec<Entry>) {
        let from_env = src
            .env_value(ENV_NAME)
            .map(str::trim)
            .filter(|v| !v.is_empty());
        let from_config = src
            .get("display.language")
            .and_then(Value::as_str)
            .map(str::trim);
        if let (Some(_), Some(_)) = (from_env, src.get("display.language")) {
            entries.push(
                Entry::new(
                    Component::Identity,
                    "display.language",
                    Disposition::Skipped,
                )
                .to("display.language")
                .because(format!("the Hermes .env sets {ENV_NAME}, which wins")),
            );
        }
        let (item, value) = match (from_env, from_config) {
            (Some(v), _) => (format!(".env:{ENV_NAME}"), v),
            (None, Some(v)) => ("display.language".to_owned(), v),
            (None, None) => return,
        };
        let entry = |d| Entry::new(Component::Identity, item.clone(), d).to("display.language");
        let language = value.to_lowercase().replace('_', "-");
        if language == pmt_i18n::ENGLISH {
            entries.push(entry(Disposition::DefaultOmitted).because("the Prometheus default"));
            return;
        }
        if let Some(why) = self.unavailable(&language) {
            entries.push(entry(Disposition::Unsupported).showing(value).because(why));
            return;
        }
        self.writes
            .key("display.language", language.as_str(), entries.len());
        entries.push(entry(Disposition::Transformed).showing(language.as_str()));
    }

    /// Why `language` cannot be selected after this migration: neither
    /// bundled nor supplied by an overlay it converts.
    fn unavailable(&self, language: &str) -> Option<String> {
        if let Err(err) = pmt_i18n::check_id(language) {
            return Some(err.message().to_owned());
        }
        let bundled = pmt_i18n::languages(&Sources::default())
            .iter()
            .any(|l| l.id == language);
        let overlay = self.overlays.iter().any(|o| {
            Surface::of_file(o.target.trim_start_matches("locales/"))
                .is_some_and(|(id, _)| id == language)
        });
        (!bundled && !overlay).then(|| {
            "no catalog for this language: install a language pack, then set display.language"
                .to_owned()
        })
    }

    /// Dry run: marks a language that conflicts with the destination's.
    pub(crate) fn predict(&self, existing: &Existing<'_>, entries: &mut [Entry]) {
        self.writes.predict(existing, entries);
    }

    /// Writes the overlays, then the language.
    ///
    /// # Errors
    ///
    /// `home.io` for an overlay that cannot be written; the config file's
    /// errors.
    pub(crate) fn apply(&self, dest: &Dest<'_>, entries: &mut [Entry]) -> Result<(), Error> {
        for overlay in &self.overlays {
            write_overlay(dest, overlay, &mut entries[overlay.entry])?;
        }
        self.writes.apply(dest, entries)
    }
}

/// Writes one overlay against the ledger, like `SOUL.md`: a file edited in
/// Prometheus since the last run is kept as a conflict.
fn write_overlay(dest: &Dest<'_>, overlay: &Overlay, entry: &mut Entry) -> Result<(), Error> {
    let path = dest.scope.dir().join(&overlay.target);
    let current = std::fs::read_to_string(&path)
        .ok()
        .map(|old| ledger::digest(old.as_bytes()));
    let new = ledger::digest(overlay.text.as_bytes());
    let key = format!("file:{}", overlay.target);
    let prior = dest.ledger.borrow().get(&key).cloned();
    let decision = ledger::decide(
        prior.as_ref(),
        &new,
        &new,
        current.as_deref(),
        dest.overwrite,
    );
    let why = format!("{} was edited in Prometheus", overlay.target);
    if writes::settle(decision, entry, &why) {
        dest.keep(&path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot create", parent, &e))?;
        }
        pmt_core::private_fs::write_atomic(&path, overlay.text.as_bytes())
            .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot write", &path, &e))?;
    }
    if matches!(decision, ledger::Decision::Write | ledger::Decision::Same) {
        dest.ledger.borrow_mut().record(key, new.clone(), new);
    }
    Ok(())
}
