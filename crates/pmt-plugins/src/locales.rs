//! Language packs (spec: i18n, Language-pack plugins).
//!
//! A pack is any plugin declaring `[[provides_locales]]`; its catalogs are
//! registered from the declared files while it is enabled and runnable,
//! and unregistered as soon as it is not (the lookup runs on every
//! survey). A pack never changes `display.language`.
//!
//! A Hermes pack (`plugin.yaml` with `provides_locales`, or a plugin that
//! calls `ctx.register_locale`/`register_locale_dir`) ships YAML catalogs:
//! they are converted to TOML catalogs when the plugin is staged for
//! install, keeping text values only.

use std::path::Path;

use pmt_i18n::{PackLocale, Surface, catalog};
use serde_json::Value;

use crate::discovery::Discovered;
use crate::load::{State, Status};
use crate::manifest::LocaleDeclaration;

/// The languages of the surveyed plugins that run, in load order.
#[must_use]
pub fn packs(surveyed: &[(Discovered, Status)]) -> Vec<PackLocale> {
    surveyed
        .iter()
        .filter(|(_, status)| status.state == State::Loaded)
        .filter_map(|(plugin, _)| Some((plugin.manifest.as_ref().ok()?, plugin.dir.as_ref()?)))
        .flat_map(|(manifest, dir)| {
            manifest.provides_locales.iter().map(|declared| PackLocale {
                plugin: manifest.id.clone(),
                id: declared.id.clone(),
                endonym: declared.endonym.clone(),
                rtl: declared.rtl,
                dir: dir.clone(),
            })
        })
        .collect()
}

/// The languages a Hermes `plugin.yaml` declares (`provides_locales`:
/// ids, or `{id, endonym, rtl}` mappings); a plugin registering catalogs
/// from code (`registers`) also provides every language its `locales/`
/// YAML files cover.
#[must_use]
pub fn hermes_declared(yaml: &Value, registers: bool, dir: &Path) -> Vec<LocaleDeclaration> {
    let mut declared: Vec<LocaleDeclaration> = yaml
        .get("provides_locales")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(declaration)
        .collect();
    if registers {
        for (id, _, _) in yaml_catalogs(dir) {
            if !declared.iter().any(|d| d.id == id) {
                declared.push(LocaleDeclaration {
                    endonym: id.clone(),
                    id,
                    rtl: false,
                });
            }
        }
    }
    declared.sort_by(|a, b| a.id.cmp(&b.id));
    declared.dedup_by(|a, b| a.id == b.id);
    declared
}

fn declaration(item: &Value) -> Option<LocaleDeclaration> {
    let (id, endonym, rtl) = match item {
        Value::String(id) => (id.clone(), None, false),
        Value::Object(map) => (
            map.get("id")?.as_str()?.to_owned(),
            map.get("endonym")
                .and_then(Value::as_str)
                .map(str::to_owned),
            map.get("rtl").and_then(Value::as_bool).unwrap_or(false),
        ),
        _ => return None,
    };
    pmt_i18n::check_id(&id).ok()?;
    Some(LocaleDeclaration {
        endonym: endonym.unwrap_or_else(|| id.clone()),
        id,
        rtl,
    })
}

/// `(id, surface, path)` of each `locales/<id>[.tui|.desktop].yaml` (or
/// `.yml`) file of `dir`.
fn yaml_catalogs(dir: &Path) -> Vec<(String, Surface, std::path::PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir.join(pmt_i18n::OVERLAY_DIR)) else {
        return Vec::new();
    };
    let mut found: Vec<_> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let stem = name
                .strip_suffix(".yaml")
                .or_else(|| name.strip_suffix(".yml"))?;
            let file = format!("{stem}.toml");
            let (id, surface) = Surface::of_file(&file)?;
            Some((id.to_owned(), surface, entry.path()))
        })
        .collect();
    found.sort();
    found
}

/// Converts the YAML catalogs of the `declared` languages under
/// `<dir>/locales/` to TOML catalogs beside them. Returns what was left
/// out: unreadable files and entries whose value is not text.
#[must_use]
pub fn convert_hermes_catalogs(dir: &Path, declared: &[LocaleDeclaration]) -> Vec<String> {
    let mut report = Vec::new();
    for (id, surface, path) in yaml_catalogs(dir) {
        if !declared.iter().any(|d| d.id == id) {
            continue;
        }
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|text| pmt_skills::yaml::parse(&text).map_err(|e| e.reason));
        let value = match parsed {
            Ok(value) => value,
            Err(reason) => {
                report.push(format!("locales/{name}: not converted ({reason})"));
                continue;
            }
        };
        let (entries, skipped) = catalog::from_json(&value);
        report.extend(
            skipped
                .iter()
                .map(|key| format!("locales/{name}: {key} skipped (not text)")),
        );
        let target = dir.join(pmt_i18n::OVERLAY_DIR).join(surface.file_name(&id));
        if let Err(err) = std::fs::write(&target, catalog::to_toml(&entries)) {
            report.push(format!("locales/{name}: not converted ({err})"));
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermes_yaml_catalogs_become_toml_with_text_values_only() {
        let dir = tempfile::tempdir().unwrap();
        let locales = dir.path().join("locales");
        std::fs::create_dir_all(&locales).unwrap();
        std::fs::write(
            locales.join("pl.yaml"),
            "approval:\n  denied: \"Odrzucono\"\n  count: 3\n",
        )
        .unwrap();
        std::fs::write(
            locales.join("pl.tui.yaml"),
            "tui:\n  background: \"[tło] {0}\"\n",
        )
        .unwrap();
        std::fs::write(locales.join("de.yaml"), "approval:\n  denied: Nein\n").unwrap();
        let yaml = serde_json::json!({"provides_locales": [{"id": "pl", "endonym": "Polski"}]});
        let declared = hermes_declared(&yaml, false, dir.path());
        assert_eq!(declared.len(), 1);
        assert_eq!(declared[0].endonym, "Polski");
        let report = convert_hermes_catalogs(dir.path(), &declared);
        assert_eq!(
            report,
            ["locales/pl.yaml: approval.count skipped (not text)"]
        );
        let core =
            catalog::parse(&std::fs::read_to_string(locales.join("pl.toml")).unwrap()).unwrap();
        assert_eq!(core["approval.denied"], "Odrzucono");
        let tui =
            catalog::parse(&std::fs::read_to_string(locales.join("pl.tui.toml")).unwrap()).unwrap();
        assert_eq!(tui["tui.background"], "[tło] {0}");
        assert!(
            !locales.join("de.toml").exists(),
            "only declared languages convert"
        );
        let registered = hermes_declared(&serde_json::json!({}), true, dir.path());
        let ids: Vec<_> = registered.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["de", "pl"]);
    }
}
