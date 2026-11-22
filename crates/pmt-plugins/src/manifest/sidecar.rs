//! `[sidecar]` of an adapter plugin (spec: platform-adapters, Supervised
//! sidecar adapters): a helper process the daemon runs beside the
//! adapter, for platforms whose client needs a runtime the WASM sandbox
//! cannot host.
//!
//! ```toml
//! [sidecar]
//! runtime = "node"                       # or "none": `command` is a file of the plugin
//! command = "sidecar/index.js"           # relative to the plugin directory
//! args = ["--port", "{port}"]            # `{port}` is the loopback port it serves
//! lockfile = "sidecar/package-lock.json" # node: the hash-pinned package set
//! secrets = ["PHOTON_TOKEN"]             # declared secrets it receives
//! ready_timeout_secs = 30
//! reads_home = false                     # its children read the user's home (a CLI's own login)
//! ```

use std::path::{Component, Path};

use toml::{Table, Value};

use super::invalid;
use pmt_core::error::Error;

/// The runtime a sidecar runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    /// The managed Node runtime runs `command` (a script).
    Node,
    /// `command` is an executable shipped with the plugin.
    None,
}

/// A parsed `[sidecar]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarDeclaration {
    /// What runs it.
    pub runtime: Runtime,
    /// The script or executable, relative to the plugin directory.
    pub command: String,
    /// Its arguments (`{port}` replaced by the port it serves).
    pub args: Vec<String>,
    /// The npm lockfile pinning its packages (`node` only), relative to
    /// the plugin directory.
    pub lockfile: Option<String>,
    /// The declared secrets passed to it as environment variables.
    pub secrets: Vec<String>,
    /// Seconds it has to accept connections on its port.
    pub ready_timeout_secs: u64,
    /// The OS sandbox lets it (and what it starts) read the user's home,
    /// credential stores excepted: for a sidecar that runs a platform CLI
    /// which keeps its own login there (Raft).
    pub reads_home: bool,
}

const KEYS: &[&str] = &[
    "runtime",
    "command",
    "args",
    "lockfile",
    "secrets",
    "ready_timeout_secs",
    "reads_home",
];

fn relative(key: &str, text: &str) -> Result<String, Error> {
    let inside = !text.is_empty()
        && Path::new(text)
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    if inside {
        Ok(text.to_owned())
    } else {
        Err(invalid(format!(
            "sidecar.{key}: '{text}' must be a path inside the plugin directory"
        )))
    }
}

fn strings(table: &Table, key: &str) -> Result<Vec<String>, Error> {
    match table.get(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| invalid(format!("sidecar.{key} must be a list of strings")))
            })
            .collect(),
        Some(_) => Err(invalid(format!("sidecar.{key} must be a list of strings"))),
    }
}

/// Parses `[sidecar]`; every secret must be one the plugin declares.
///
/// # Errors
///
/// `plugins.manifest` naming the field.
pub fn parse(
    value: Option<&Value>,
    declared_secrets: &[String],
) -> Result<Option<SidecarDeclaration>, Error> {
    let Some(value) = value else {
        return Ok(None);
    };
    let table = value
        .as_table()
        .ok_or_else(|| invalid("sidecar must be a table".to_owned()))?;
    if let Some(key) = table.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(invalid(format!("unknown sidecar field '{key}'")));
    }
    let runtime = match table
        .get("runtime")
        .and_then(Value::as_str)
        .unwrap_or("none")
    {
        "node" => Runtime::Node,
        "none" => Runtime::None,
        other => {
            return Err(invalid(format!(
                "sidecar.runtime: unknown runtime '{other}' (expected node or none)"
            )));
        }
    };
    let command = table
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("sidecar.command is required".to_owned()))?;
    let lockfile = match table.get("lockfile").and_then(Value::as_str) {
        Some(path) => Some(relative("lockfile", path)?),
        None if runtime == Runtime::Node => {
            return Err(invalid(
                "sidecar.lockfile is required with runtime = \"node\" (its packages must be hash-pinned)"
                    .to_owned(),
            ));
        }
        None => None,
    };
    let secrets = strings(table, "secrets")?;
    if let Some(undeclared) = secrets.iter().find(|s| !declared_secrets.contains(s)) {
        return Err(invalid(format!(
            "sidecar.secrets: '{undeclared}' is not in capabilities.secrets"
        )));
    }
    let ready_timeout_secs = match table.get("ready_timeout_secs") {
        None => 30,
        Some(Value::Integer(n)) if (1..=600).contains(n) => n.unsigned_abs(),
        Some(_) => {
            return Err(invalid(
                "sidecar.ready_timeout_secs must be 1 to 600".to_owned(),
            ));
        }
    };
    Ok(Some(SidecarDeclaration {
        runtime,
        command: relative("command", command)?,
        args: strings(table, "args")?,
        lockfile,
        secrets,
        ready_timeout_secs,
        reads_home: match table.get("reads_home") {
            None => false,
            Some(Value::Boolean(on)) => *on,
            Some(_) => return Err(invalid("sidecar.reads_home must be a boolean".to_owned())),
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Result<Option<SidecarDeclaration>, Error> {
        let table: Table = text.parse().unwrap();
        parse(table.get("sidecar"), &["TOKEN".to_owned()])
    }

    #[test]
    fn a_sidecar_names_a_command_inside_the_plugin_and_declared_secrets() {
        let s = parsed(
            "[sidecar]\ncommand = \"bin/side\"\nargs = [\"{port}\"]\nsecrets = [\"TOKEN\"]\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!((s.runtime, s.ready_timeout_secs), (Runtime::None, 30));
        assert!(parsed("[sidecar]\ncommand = \"../x\"\n").is_err());
        assert!(parsed("[sidecar]\ncommand = \"x\"\nsecrets = [\"OTHER\"]\n").is_err());
        let node = parsed("[sidecar]\nruntime = \"node\"\ncommand = \"index.js\"\n").unwrap_err();
        assert!(node.message().contains("hash-pinned"), "{}", node.message());
    }
}
