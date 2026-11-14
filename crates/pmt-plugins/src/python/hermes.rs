//! Python plugin manifests (spec: python-bridge, Scope of hosted code;
//! Hermes API compatibility level): a Hermes `plugin.yaml` translated into
//! a `kind = "python"` [`Manifest`], and the `[python]` launch table both
//! spellings share.
//!
//! A `plugin.yaml` declares no capabilities, so the translation asks for
//! what the static audit infers from the source (secrets it reads, hosts
//! its URLs name, hooks, commands, providers...) plus the manifest's own
//! `requires_env` and `provides_hooks`; the user approves that list like
//! any plugin's on first enable.

use std::path::Path;

use pmt_core::error::{Error, codes};
use semver::{Version, VersionReq};
use serde_json::Value;
use toml::Table;

use super::audit::{self, Inferred};
use crate::manifest::{Declared, Kind, LlmDeclaration, MANIFEST_VERSION, Manifest, check_id};

/// The Hermes manifest file.
pub const HERMES_MANIFEST: &str = "plugin.yaml";

/// Hermes plugin kinds.
const HERMES_KINDS: &[&str] = &[
    "standalone",
    "backend",
    "exclusive",
    "platform",
    "model-provider",
];

/// The `[python]` launch table of a python plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PythonLaunch {
    /// The Hermes kind (`standalone`, `model-provider`, ...).
    pub hermes_kind: String,
    /// `requires_hermes`, as written (empty: any).
    pub requires_hermes: String,
    /// Requirements declared inline (`pip_dependencies`); a
    /// `requirements.txt` or `pyproject.toml` in the plugin directory is
    /// read by the bridge.
    pub requirements: Vec<String>,
}

impl PythonLaunch {
    /// The launch table of `manifest` (defaults when absent).
    #[must_use]
    pub fn of(manifest: &Manifest) -> Self {
        let Some(table) = &manifest.launch else {
            return Self {
                hermes_kind: "standalone".to_owned(),
                ..Self::default()
            };
        };
        let text = |key: &str| {
            table
                .get(key)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned()
        };
        let kind = text("hermes_kind");
        Self {
            hermes_kind: if kind.is_empty() {
                "standalone".to_owned()
            } else {
                kind
            },
            requires_hermes: text("requires_hermes"),
            requirements: table
                .get("requirements")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// Whether it is a Hermes model-provider plugin.
    #[must_use]
    pub fn is_model_provider(&self) -> bool {
        self.hermes_kind == "model-provider"
    }

    /// `requires Hermes <range>; bridge implements <level>` when its range
    /// excludes the bridge's compat level.
    #[must_use]
    pub fn compat_problem(&self) -> Option<String> {
        let level = super::api::HERMES_COMPAT_LEVEL;
        (!self.requires_hermes.is_empty() && !super::api::admits(&self.requires_hermes, level))
            .then(|| {
                format!(
                    "requires Hermes {}; bridge implements {level}",
                    self.requires_hermes
                )
            })
    }
}

/// Reads `<dir>/plugin.yaml` as a python plugin named after `dir`.
///
/// # Errors
///
/// `plugins.manifest` for unreadable YAML, an invalid id or an unknown
/// kind; `plugins.discovery` when files cannot be read.
pub fn read(dir: &Path) -> Result<Manifest, Error> {
    let id = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    read_as(dir, id)
}

/// Reads `<dir>/plugin.yaml` of a plugin being installed: its id is the
/// manifest's `name` (lower-cased, other characters as `-`), since the
/// staging directory's name means nothing.
///
/// # Errors
///
/// As [`read`].
pub fn read_staged(dir: &Path) -> Result<Manifest, Error> {
    let path = dir.join(HERMES_MANIFEST);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, &path, &e))?;
    let yaml = pmt_skills::yaml::parse(&text).unwrap_or(Value::Null);
    let name = yaml.get("name").and_then(Value::as_str).unwrap_or("");
    let id: String = name
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    read_as(dir, id.trim_matches('-').to_owned())
}

fn read_as(dir: &Path, id: String) -> Result<Manifest, Error> {
    let path = dir.join(HERMES_MANIFEST);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, &path, &e))?;
    let yaml = pmt_skills::yaml::parse(&text).map_err(|e| {
        invalid(format!(
            "invalid plugin.yaml (line {}): {}",
            e.line, e.reason
        ))
    })?;
    check_id(&id)?;
    let field = |key: &str| {
        yaml.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let hermes_kind = match field("kind").as_str() {
        "" => "standalone".to_owned(),
        k if HERMES_KINDS.contains(&k) => k.to_owned(),
        other => return Err(invalid(format!("unknown Hermes plugin kind '{other}'"))),
    };
    let declared_hooks = list(&yaml, "provides_hooks");
    let report = audit::audit(dir, &[])?;
    let mut launch = Table::new();
    launch.insert("hermes_kind".into(), hermes_kind.clone().into());
    launch.insert("requires_hermes".into(), field("requires_hermes").into());
    let mut requirements = list(&yaml, "pip_dependencies");
    requirements.extend(list(&yaml, "python_dependencies"));
    launch.insert(
        "requirements".into(),
        toml::Value::Array(requirements.into_iter().map(Into::into).collect()),
    );
    let mut secrets = list(&yaml, "requires_env");
    secrets.extend(list(&yaml, "optional_env"));
    let capabilities = declared(
        &report.inferred,
        &declared_hooks,
        &secrets,
        &hermes_kind,
        &id,
    );
    Ok(Manifest {
        manifest_version: MANIFEST_VERSION,
        version: version(&field("version")),
        kind: Kind::Python,
        description: field("description"),
        license: field("license"),
        requires_prometheus: VersionReq::STAR,
        worlds: Vec::new(),
        capabilities,
        requires_plugins: Vec::new(),
        kanban_lanes: Vec::new(),
        automation_blueprints: Vec::new(),
        delegation_router: None,
        provides_locales: crate::locales::hermes_declared(
            &yaml,
            report.inferred.registers_locales,
            dir,
        ),
        dashboard: None,
        ui: None,
        build: None,
        launch: Some(launch),
        id,
    })
}

fn invalid(message: String) -> Error {
    Error::new(&codes::PLUGINS_MANIFEST, message)
}

/// A YAML list of strings (`requires_env` entries may be `{name: ...}`).
fn list(yaml: &Value, key: &str) -> Vec<String> {
    yaml.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| match item {
                    Value::String(s) => Some(s.clone()),
                    Value::Object(map) => {
                        map.get("name").and_then(Value::as_str).map(str::to_owned)
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A lenient semantic version: `1.0` is `1.0.0`; anything unreadable is
/// `0.0.0`.
fn version(text: &str) -> Version {
    let text = text.trim().trim_start_matches('v');
    if let Ok(v) = Version::parse(text) {
        return v;
    }
    let parts: Vec<u64> = text.split('.').map_while(|p| p.parse().ok()).collect();
    match parts.as_slice() {
        [major] => Version::new(*major, 0, 0),
        [major, minor, ..] => Version::new(*major, *minor, parts.get(2).copied().unwrap_or(0)),
        [] => Version::new(0, 0, 0),
    }
}

/// The capabilities a `plugin.yaml` plugin asks for.
fn declared(
    inferred: &Inferred,
    hooks: &[String],
    secrets: &[String],
    kind: &str,
    id: &str,
) -> Declared {
    let known = |hook: &&String| pmt_core::hooks::check(hook).is_ok();
    let mut all_hooks: Vec<String> = hooks
        .iter()
        .chain(&inferred.hooks)
        .filter(known)
        .cloned()
        .collect();
    all_hooks.sort();
    all_hooks.dedup();
    let mut all_secrets: Vec<String> = secrets.iter().chain(&inferred.secrets).cloned().collect();
    all_secrets.sort();
    all_secrets.dedup();
    let mut providers: Vec<String> = inferred.providers.iter().cloned().collect();
    if kind == "model-provider" && providers.is_empty() {
        providers.push(id.to_owned());
    }
    let mut cli_commands: Vec<String> = inferred.cli_commands.iter().cloned().collect();
    if inferred.memory_cli && !cli_commands.iter().any(|c| c == id) {
        cli_commands.push(id.to_owned());
    }
    // A profile reusing a built-in id replaces it, which needs its own grant.
    let builtin = crate::surfaces::builtin_names();
    let providers_override = providers
        .iter()
        .filter(|p| builtin.contains(p))
        .cloned()
        .collect();
    Declared {
        network: inferred.hosts.iter().cloned().collect(),
        secrets: all_secrets,
        llm: inferred.llm.then(|| LlmDeclaration {
            enabled: true,
            ..LlmDeclaration::default()
        }),
        hooks: all_hooks,
        prompt_section: inferred
            .prompt_section
            .then_some(pmt_core::capability::MAX_PROMPT_SECTION_CHARS),
        commands: inferred.commands.iter().cloned().collect(),
        cli_commands,
        providers,
        providers_override,
        approval_transport: inferred.approval_transport,
        inject_messages: inferred.inject_messages,
        platform_actions: inferred.platform_actions.iter().cloned().collect(),
        subprocess: inferred.subprocess,
        ..Declared::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hermes_manifest_becomes_a_python_plugin_asking_for_what_it_uses() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("spotify");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            "name: spotify\nversion: \"1.2\"\ndescription: Play music\nrequires_env:\n  - SPOTIFY_TOKEN\nprovides_hooks:\n  - post_tool_call\nrequires_hermes: \">=2026.1\"\npip_dependencies:\n  - requests>=2.31,<3\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("__init__.py"),
            "URL = 'https://api.spotify.com/v1'\ndef register(ctx):\n    pass\n",
        )
        .unwrap();
        let manifest = read(&dir).unwrap();
        assert_eq!(manifest.kind, Kind::Python);
        assert_eq!(manifest.id, "spotify");
        assert_eq!(manifest.version, Version::new(1, 2, 0));
        assert_eq!(manifest.capabilities.secrets, ["SPOTIFY_TOKEN"]);
        assert_eq!(manifest.capabilities.hooks, ["post_tool_call"]);
        assert_eq!(manifest.capabilities.network, ["api.spotify.com"]);
        let launch = PythonLaunch::of(&manifest);
        assert_eq!(launch.requirements, ["requests>=2.31,<3"]);
        assert_eq!(launch.compat_problem(), None);
    }

    #[test]
    fn a_newer_hermes_api_is_named() {
        let launch = PythonLaunch {
            requires_hermes: ">=2027.1".to_owned(),
            ..PythonLaunch::default()
        };
        assert_eq!(
            launch.compat_problem().as_deref(),
            Some("requires Hermes >=2027.1; bridge implements 2026.10")
        );
    }
}
