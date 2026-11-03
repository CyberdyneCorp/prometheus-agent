//! `plugin.toml` (spec: plugin-system, Plugin kinds and manifest).
//!
//! ```toml
//! manifest_version = 1
//! id = "weather"
//! version = "1.0.0"
//! kind = "wasm"
//! description = "Forecasts for the agent."
//! license = "MIT"
//! requires_prometheus = ">=0.1"
//! worlds = ["tool@1.0"]
//!
//! [capabilities]
//! network = ["api.weather.example"]
//! ```
//!
//! Every unknown key (top level or capability), kind or world is refused
//! with an error naming it, a `manifest_version` above
//! [`MANIFEST_VERSION`] asks for a newer Prometheus, and a
//! `requires_prometheus` range that excludes the running version is
//! refused before anything is instantiated. A `wasm` plugin's component is
//! the file [`COMPONENT_FILE`] next to the manifest.

mod capabilities;

use std::path::Path;

use pmt_core::error::{Error, codes};
use semver::{Version, VersionReq};
use toml::{Table, Value};

pub use capabilities::{Declared, LlmDeclaration, describe, label};

use crate::worlds::{self, Support, WorldRef};

/// The manifest file of a plugin directory.
pub const MANIFEST_FILE: &str = "plugin.toml";
/// The Hermes manifest the python bridge translates (kind `python`).
pub const HERMES_MANIFEST_FILE: &str = "plugin.yaml";
/// The component of a `wasm` plugin.
pub const COMPONENT_FILE: &str = "plugin.wasm";
/// The newest `manifest_version` this host reads.
pub const MANIFEST_VERSION: u64 = 1;

/// Every top-level key `plugin.toml` may hold. `build` is the dev loop's
/// build command; `mcp` and `python` are the launch tables of those kinds
/// (their keys are owned by the MCP client and the python bridge).
const FIELDS: &[&str] = &[
    "manifest_version",
    "id",
    "version",
    "kind",
    "description",
    "license",
    "requires_prometheus",
    "worlds",
    "capabilities",
    "requires_plugins",
    "kanban_lanes",
    "build",
    "mcp",
    "python",
];

/// How a plugin runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A WebAssembly component in the host.
    Wasm,
    /// An MCP server.
    Mcp,
    /// A Python plugin hosted by the python bridge.
    Python,
}

impl Kind {
    /// The manifest spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wasm => "wasm",
            Self::Mcp => "mcp",
            Self::Python => "python",
        }
    }
}

/// A parsed, validated manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    /// `manifest_version`.
    pub manifest_version: u64,
    /// `^[a-z0-9][a-z0-9_-]{0,63}$`.
    pub id: String,
    /// The plugin's own version.
    pub version: Version,
    /// How it runs.
    pub kind: Kind,
    /// One line for listings.
    pub description: String,
    /// SPDX expression.
    pub license: String,
    /// Prometheus versions it runs on.
    pub requires_prometheus: VersionReq,
    /// The WIT worlds it implements (`wasm`), each checked against the host.
    pub worlds: Vec<(WorldRef, Support)>,
    /// The authority it asks for.
    pub capabilities: Declared,
    /// Plugins that must load first.
    pub requires_plugins: Vec<String>,
    /// `[[kanban_lanes]]`, kept for the kanban worker lanes.
    pub kanban_lanes: Vec<Table>,
    /// The dev loop's build command.
    pub build: Option<String>,
    /// `[mcp]` or `[python]`: how the server or bridge starts it.
    pub launch: Option<Table>,
}

impl Manifest {
    /// Reads `<dir>/plugin.toml` and checks it against `running` (the
    /// Prometheus version).
    ///
    /// # Errors
    ///
    /// `plugins.manifest` (65) naming the field; `plugins.discovery` when
    /// the file cannot be read.
    pub fn read(dir: &Path, running: &Version) -> Result<Self, Error> {
        let path = dir.join(MANIFEST_FILE);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, &path, &e))?;
        Self::parse(&text, running)
    }

    /// Parses manifest text.
    ///
    /// # Errors
    ///
    /// `plugins.manifest` (65) naming the offending field or value.
    pub fn parse(text: &str, running: &Version) -> Result<Self, Error> {
        let table: Table = text.parse().map_err(|e: toml::de::Error| {
            invalid(format!("invalid plugin.toml: {}", e.message()))
        })?;
        if let Some(key) = table.keys().find(|k| !FIELDS.contains(&k.as_str())) {
            return Err(invalid(format!("unknown manifest field '{key}'")));
        }
        let manifest_version = integer(&table, "manifest_version")?;
        if manifest_version > MANIFEST_VERSION {
            return Err(invalid(format!(
                "requires a newer Prometheus (manifest_version {manifest_version})"
            )));
        }
        let id = string(&table, "id")?;
        check_id(&id)?;
        let kind = kind(&string(&table, "kind")?)?;
        let requires_prometheus = requirement(&table)?;
        if !requires_prometheus.matches(running) {
            return Err(invalid(format!(
                "plugin '{id}' requires Prometheus {requires_prometheus} (this is {running})"
            )));
        }
        let launch = launch_table(&table, kind)?;
        Ok(Self {
            manifest_version,
            version: Version::parse(&string(&table, "version")?)
                .map_err(|e| invalid(format!("version: {e}")))?,
            kind,
            description: string(&table, "description")?,
            license: string(&table, "license")?,
            requires_prometheus,
            worlds: world_list(&table)?,
            capabilities: Declared::parse(table.get("capabilities"))?,
            requires_plugins: strings(&table, "requires_plugins")?,
            kanban_lanes: tables(&table, "kanban_lanes")?,
            build: optional_string(&table, "build")?,
            launch,
            id,
        })
    }
}

/// A `plugins.manifest` error.
pub(crate) fn invalid(message: String) -> Error {
    Error::new(&codes::PLUGINS_MANIFEST, message)
}

/// Checks `^[a-z0-9][a-z0-9_-]{0,63}$`.
///
/// # Errors
///
/// `plugins.manifest`: `invalid plugin id '<id>'`.
pub fn check_id(id: &str) -> Result<(), Error> {
    let mut chars = id.chars();
    let valid = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'));
    if valid {
        Ok(())
    } else {
        Err(invalid(format!(
            "invalid plugin id '{id}' (lower-case letters, digits, '_' and '-', at most 64)"
        )))
    }
}

fn kind(text: &str) -> Result<Kind, Error> {
    match text {
        "wasm" => Ok(Kind::Wasm),
        "mcp" => Ok(Kind::Mcp),
        "python" => Ok(Kind::Python),
        other => Err(invalid(format!(
            "unknown plugin kind '{other}' (expected wasm, mcp or python)"
        ))),
    }
}

fn requirement(table: &Table) -> Result<VersionReq, Error> {
    let text = string(table, "requires_prometheus")?;
    VersionReq::parse(&text).map_err(|e| invalid(format!("requires_prometheus: {e}")))
}

fn world_list(table: &Table) -> Result<Vec<(WorldRef, Support)>, Error> {
    strings(table, "worlds")?
        .iter()
        .map(|text| {
            let world: WorldRef = text.parse()?;
            let support = worlds::check(&world)?;
            Ok((world, support))
        })
        .collect()
}

fn launch_table(table: &Table, kind: Kind) -> Result<Option<Table>, Error> {
    for (key, owner) in [("mcp", Kind::Mcp), ("python", Kind::Python)] {
        if table.contains_key(key) && kind != owner {
            return Err(invalid(format!(
                "[{key}] is only valid for kind = \"{key}\" (this plugin is {})",
                kind.as_str()
            )));
        }
    }
    let key = match kind {
        Kind::Wasm => return Ok(None),
        Kind::Mcp => "mcp",
        Kind::Python => "python",
    };
    match table.get(key) {
        None => Ok(None),
        Some(Value::Table(t)) => Ok(Some(t.clone())),
        Some(_) => Err(invalid(format!("{key} must be a table"))),
    }
}

fn integer(table: &Table, key: &str) -> Result<u64, Error> {
    match table.get(key) {
        Some(Value::Integer(n)) => {
            u64::try_from(*n).map_err(|_| invalid(format!("{key} must be a positive integer")))
        }
        Some(_) => Err(invalid(format!("{key} must be an integer"))),
        None => Err(missing(key)),
    }
}

fn string(table: &Table, key: &str) -> Result<String, Error> {
    optional_string(table, key)?.ok_or_else(|| missing(key))
}

fn optional_string(table: &Table, key: &str) -> Result<Option<String>, Error> {
    match table.get(key) {
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(invalid(format!("{key} must be a string"))),
        None => Ok(None),
    }
}

pub(crate) fn strings(table: &Table, key: &str) -> Result<Vec<String>, Error> {
    match table.get(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| invalid(format!("{key} must be a list of strings")))
            })
            .collect(),
        Some(_) => Err(invalid(format!("{key} must be a list of strings"))),
    }
}

fn tables(table: &Table, key: &str) -> Result<Vec<Table>, Error> {
    match table.get(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_table()
                    .cloned()
                    .ok_or_else(|| invalid(format!("{key} must be an array of tables")))
            })
            .collect(),
        Some(_) => Err(invalid(format!("{key} must be an array of tables"))),
    }
}

fn missing(key: &str) -> Error {
    invalid(format!("missing manifest field '{key}'"))
}

#[cfg(test)]
mod tests;
