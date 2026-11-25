//! Shareable plugin collections (spec: plugin-system, Plugin collections):
//! `prometheus-collection.toml`, and Hermes `hermes-pack.yaml` translated
//! into the same shape.
//!
//! A collection lists plugins by catalog `name` or by `repo` (with an
//! optional `subdir`), each pinned to a full 40-hex commit SHA, optional
//! non-secret `config` seeds for `plugins.entries.<id>`, and a `skills`
//! list. It never carries grants: every plugin still goes through the
//! normal install and capability-grant flow. Parsing validates the whole
//! file before anything is fetched, so a bad entry stops the collection
//! up front, naming the entry.

use std::collections::BTreeMap;

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::catalog::is_commit;

/// The collection file name.
pub const FILE_NAME: &str = "prometheus-collection.toml";
/// The Hermes pack file name, translated on read.
pub const HERMES_PACK: &str = "hermes-pack.yaml";

/// Keys a collection entry or seed may never set: consent travels only
/// through the grant flow.
const GRANT_KEYS: &[&str] = &[
    "grants",
    "grant",
    "granted_capabilities",
    "capabilities_consent",
];

/// Fragments that make a seed key secret-shaped (case-insensitive).
const SECRET_FRAGMENTS: &[&str] = &["token", "key", "secret", "password"];

/// One collection.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Collection {
    /// Its name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// One line about it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Skill ids it suggests (installed with `prometheus skills install`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// The plugins, in install order.
    #[serde(default, rename = "plugins")]
    pub plugins: Vec<Item>,
}

/// One pinned plugin of a collection.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    /// A catalog id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A Git repository URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// The plugin's directory inside `repo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
    /// The full commit SHA.
    #[serde(default)]
    pub r#ref: String,
    /// Seeds for `plugins.entries.<id>` (existing values win).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, Value>,
}

impl Item {
    /// What messages call it: the catalog name, else `repo[/subdir]`.
    #[must_use]
    pub fn label(&self) -> String {
        match (&self.name, &self.repo, &self.subdir) {
            (Some(name), _, _) => name.clone(),
            (None, Some(repo), Some(subdir)) => format!("{repo}/{subdir}"),
            (None, Some(repo), None) => repo.clone(),
            (None, None, _) => String::new(),
        }
    }

    /// `catalog <name>` or `git <repo>[#subdir]`.
    #[must_use]
    pub fn source(&self) -> String {
        match (&self.name, &self.repo) {
            (Some(name), _) => format!("catalog {name}"),
            (None, Some(repo)) => match &self.subdir {
                Some(subdir) => format!("git {repo}#{subdir}"),
                None => format!("git {repo}"),
            },
            (None, None) => String::new(),
        }
    }
}

/// Whether a seed key looks like it holds a secret (`*token*`, `*key*`,
/// `*secret*`, `*password*`).
#[must_use]
pub fn is_secret_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SECRET_FRAGMENTS.iter().any(|f| lower.contains(f))
}

fn invalid(message: String) -> Error {
    Error::new(&codes::PLUGINS_COLLECTION, message)
}

impl Collection {
    /// Reads a collection file: TOML, or a Hermes pack when the name ends
    /// in `.yaml`/`.yml`.
    ///
    /// # Errors
    ///
    /// `plugins.collection` for unparsable text or an invalid entry.
    pub fn parse(file_name: &str, text: &str) -> Result<Self, Error> {
        let yaml = std::path::Path::new(file_name)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"));
        let collection = if yaml {
            Self::from_hermes_pack(text)?
        } else {
            toml::from_str::<Self>(text)
                .map_err(|e| invalid(format!("{file_name}: {}", e.message())))?
        };
        collection.validate()?;
        Ok(collection)
    }

    /// Translates a Hermes pack: `plugins: [{name|repo, ref, subdir}]`,
    /// `config: {<plugin>: {...}}` (moved onto the matching entry) and
    /// `skills`.
    ///
    /// # Errors
    ///
    /// `plugins.collection` for invalid YAML or shapes.
    pub fn from_hermes_pack(text: &str) -> Result<Self, Error> {
        let yaml = pmt_skills::yaml::parse(text)
            .map_err(|e| invalid(format!("{HERMES_PACK} (line {}): {}", e.line, e.reason)))?;
        let mut pack: Self = serde_json::from_value(serde_json::json!({
            "name": yaml.get("name").cloned().unwrap_or(Value::Null).as_str().unwrap_or(""),
            "description": yaml.get("description").and_then(Value::as_str).unwrap_or(""),
            "skills": yaml.get("skills").cloned().unwrap_or_else(|| serde_json::json!([])),
        }))
        .map_err(|e| invalid(format!("{HERMES_PACK}: {e}")))?;
        let plugins = yaml.get("plugins").and_then(Value::as_array);
        for (at, raw) in plugins.into_iter().flatten().enumerate() {
            pack.plugins.push(hermes_item(raw, at)?);
        }
        let seeds = yaml.get("config").and_then(Value::as_object);
        for (plugin, seed) in seeds.into_iter().flatten() {
            let Some(item) = pack.plugins.iter_mut().find(|i| i.plugin_id() == *plugin) else {
                return Err(invalid(format!(
                    "{HERMES_PACK}: config for '{plugin}', which the pack does not list"
                )));
            };
            let Some(table) = seed.as_object() else {
                return Err(invalid(format!(
                    "{HERMES_PACK}: config for '{plugin}' must be a mapping"
                )));
            };
            item.config
                .extend(table.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        Ok(pack)
    }

    /// Checks every entry: a catalog name or repo, a full commit SHA, a
    /// relative `subdir`, no grant keys and no secret-shaped seed keys.
    ///
    /// # Errors
    ///
    /// `plugins.collection` naming the first bad entry.
    pub fn validate(&self) -> Result<(), Error> {
        for item in &self.plugins {
            check_item(item)?;
        }
        Ok(())
    }
}

impl Item {
    /// The plugin id the entry is expected to install: the catalog name,
    /// else the last path segment of `subdir` or `repo`.
    #[must_use]
    pub fn plugin_id(&self) -> String {
        if let Some(name) = &self.name {
            return name.clone();
        }
        let path = self
            .subdir
            .as_deref()
            .or(self.repo.as_deref())
            .unwrap_or("");
        let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        last.strip_suffix(".git").unwrap_or(last).to_owned()
    }
}

fn hermes_item(raw: &Value, at: usize) -> Result<Item, Error> {
    let label = raw
        .get("name")
        .or_else(|| raw.get("repo"))
        .and_then(Value::as_str)
        .map_or_else(|| format!("#{}", at + 1), str::to_owned);
    let Some(table) = raw.as_object() else {
        return Err(invalid(format!("entry '{label}': must be a mapping")));
    };
    let text = |key: &str| table.get(key).and_then(Value::as_str).map(str::to_owned);
    for key in table.keys() {
        if !["name", "repo", "subdir", "ref", "version", "config"].contains(&key.as_str()) {
            return Err(invalid(format!("entry '{label}': unknown field '{key}'")));
        }
    }
    let config = table
        .get("config")
        .and_then(Value::as_object)
        .map(|c| c.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    Ok(Item {
        name: text("name"),
        repo: text("repo").map(|r| github_shorthand(&r)),
        subdir: text("subdir").map(|s| s.trim_matches('/').to_owned()),
        r#ref: text("ref").or_else(|| text("version")).unwrap_or_default(),
        config,
    })
}

/// Hermes accepts `owner/repo`; Prometheus installs need a URL.
fn github_shorthand(repo: &str) -> String {
    if repo.contains("://") || repo.starts_with("git@") || repo.matches('/').count() != 1 {
        repo.to_owned()
    } else {
        format!("https://github.com/{repo}")
    }
}

fn check_item(item: &Item) -> Result<(), Error> {
    let label = item.label();
    if item.name.is_none() && item.repo.is_none() {
        return Err(invalid(
            "a collection entry needs a catalog `name` or a `repo`".to_owned(),
        ));
    }
    if item.name.is_some() && item.subdir.is_some() {
        return Err(invalid(format!(
            "entry '{label}': `subdir` goes with `repo`, not a catalog name"
        )));
    }
    if !is_commit(&item.r#ref) {
        return Err(invalid(format!(
            "entry '{label}': ref must be a full commit SHA"
        )));
    }
    if let Some(subdir) = &item.subdir
        && (subdir.is_empty()
            || subdir
                .split('/')
                .any(|part| part.is_empty() || part == "..")
            || subdir.starts_with('/'))
    {
        return Err(invalid(format!(
            "entry '{label}': subdir must be a relative path inside the repository"
        )));
    }
    for key in item.config.keys() {
        if GRANT_KEYS.contains(&key.as_str()) {
            return Err(invalid(format!(
                "entry '{label}': collections cannot carry grants ('{key}'); capabilities are approved per plugin on install"
            )));
        }
        if is_secret_key(key) {
            return Err(invalid(format!(
                "entry '{label}': config seed '{key}' looks like a secret; collections carry no secrets"
            )));
        }
    }
    Ok(())
}

/// The seeds of `config` safe to export: secret-shaped and grant keys
/// stripped.
#[must_use]
pub fn exportable_seeds(config: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    config
        .iter()
        .filter(|(k, _)| !is_secret_key(k) && !GRANT_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// The collection as `prometheus-collection.toml` text.
///
/// # Errors
///
/// `plugins.collection` when a seed value has no TOML form (a JSON null).
pub fn to_toml(collection: &Collection) -> Result<String, Error> {
    toml::to_string(collection).map_err(|e| invalid(format!("cannot write the collection: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn a_tag_ref_is_refused_naming_the_entry() {
        let text = "[[plugins]]\nname = \"weather\"\nref = \"v1.2.0\"\n";
        let err = Collection::parse(FILE_NAME, text).unwrap_err();
        assert_eq!(
            err.message(),
            "entry 'weather': ref must be a full commit SHA"
        );
    }

    #[test]
    fn secret_seeds_and_grants_are_refused() {
        for key in [
            "api_token",
            "API_KEY",
            "client_secret",
            "password",
            "grants",
        ] {
            let text = format!(
                "[[plugins]]\nname = \"weather\"\nref = \"{SHA}\"\n[plugins.config]\n{key} = \"x\"\n"
            );
            let err = Collection::parse(FILE_NAME, &text).unwrap_err();
            assert!(
                err.message().starts_with("entry 'weather': "),
                "{key}: {err}"
            );
        }
    }

    #[test]
    fn a_hermes_pack_is_translated() {
        let text = format!(
            "name: starter\nplugins:\n  - name: weather\n    ref: {SHA}\n  - repo: acme/tools\n    subdir: plugins/notes\n    ref: {SHA}\nconfig:\n  notes:\n    folder: inbox\nskills:\n  - github-pr\n"
        );
        let pack = Collection::parse(HERMES_PACK, &text).unwrap();
        assert_eq!(pack.name, "starter");
        assert_eq!(pack.skills, ["github-pr"]);
        assert_eq!(
            pack.plugins[1].repo.as_deref(),
            Some("https://github.com/acme/tools")
        );
        assert_eq!(pack.plugins[1].plugin_id(), "notes");
        assert_eq!(pack.plugins[1].config["folder"], "inbox");
    }

    #[test]
    fn export_strips_secret_shaped_seeds() {
        let config: BTreeMap<String, Value> = [
            ("units".to_owned(), Value::from("metric")),
            ("api_key".to_owned(), Value::from("s3cret")),
        ]
        .into();
        let seeds = exportable_seeds(&config);
        assert_eq!(seeds.keys().collect::<Vec<_>>(), ["units"]);
    }
}
