//! `[[provides_locales]]`: the UI languages a language-pack plugin ships
//! (spec: i18n, Language-pack plugins).
//!
//! ```toml
//! [[provides_locales]]
//! id = "pl"           # a display.language id
//! endonym = "Polski"  # default: the id
//! rtl = false         # default
//! ```
//!
//! The catalogs are `locales/<id>.toml`, `locales/<id>.tui.toml` and
//! `locales/<id>.desktop.toml` next to the manifest.

use pmt_core::error::Error;
use toml::Value;

use super::invalid;

/// One declared language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocaleDeclaration {
    /// The language id.
    pub id: String,
    /// The language's own name.
    pub endonym: String,
    /// Written right to left.
    pub rtl: bool,
}

const KEYS: &[&str] = &["id", "endonym", "rtl"];

/// Parses `[[provides_locales]]` (absent = none).
///
/// # Errors
///
/// `plugins.manifest` naming an unknown key, a malformed id or a value of
/// the wrong type.
pub(super) fn parse(value: Option<&Value>) -> Result<Vec<LocaleDeclaration>, Error> {
    let items = match value {
        None => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => {
            return Err(invalid(
                "provides_locales must be an array of tables".to_owned(),
            ));
        }
    };
    let mut declared: Vec<LocaleDeclaration> = Vec::new();
    for item in items {
        let table = item
            .as_table()
            .ok_or_else(|| invalid("provides_locales must be an array of tables".to_owned()))?;
        if let Some(key) = table.keys().find(|k| !KEYS.contains(&k.as_str())) {
            return Err(invalid(format!("unknown field 'provides_locales.{key}'")));
        }
        let id = match table.get("id") {
            Some(Value::String(id)) => id.clone(),
            _ => return Err(invalid("provides_locales.id must be a string".to_owned())),
        };
        pmt_i18n::check_id(&id)
            .map_err(|_| invalid(format!("provides_locales: invalid language id '{id}'")))?;
        if declared.iter().any(|d| d.id == id) {
            return Err(invalid(format!(
                "provides_locales: '{id}' is declared twice"
            )));
        }
        let endonym = match table.get("endonym") {
            None => id.clone(),
            Some(Value::String(name)) => name.clone(),
            Some(_) => {
                return Err(invalid(
                    "provides_locales.endonym must be a string".to_owned(),
                ));
            }
        };
        let rtl = match table.get("rtl") {
            None => false,
            Some(Value::Boolean(rtl)) => *rtl,
            Some(_) => return Err(invalid("provides_locales.rtl must be a boolean".to_owned())),
        };
        declared.push(LocaleDeclaration { id, endonym, rtl });
    }
    Ok(declared)
}
