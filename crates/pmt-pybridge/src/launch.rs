//! How a sidecar process starts (spec: python-bridge, Secrets and
//! environment isolation; Capability grants enforced at the process
//! boundary): the plugin environment's interpreter in isolated mode on the
//! shim's entry script, with an environment holding only `PATH`, `HOME`
//! (the plugin's private data directory), `LANG`, `TMPDIR` (its private
//! temporary directory), `PYTHONNOUSERSITE=1`, the granted secrets and,
//! when it may reach the network, the egress proxy. The embedder wraps
//! the command in the OS sandbox.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use crate::shim::ENTRY;

/// The environment of one sidecar.
#[derive(Debug, Clone, Default)]
pub struct LaunchEnv {
    /// `HOME`: `<profile>/plugin-data/<id>`.
    pub home: PathBuf,
    /// `TMPDIR`: a private temporary directory.
    pub tmp: PathBuf,
    /// Directories put first on `PATH` (the environment's `bin`).
    pub path: Vec<PathBuf>,
    /// Granted secrets, by name.
    pub secrets: Vec<(String, String)>,
    /// `http://127.0.0.1:<port>` of the egress proxy, when networked.
    pub proxy: Option<String>,
}

/// The system directories every sidecar's `PATH` ends with.
const SYSTEM_PATH: &[&str] = if cfg!(windows) {
    &[]
} else {
    &["/usr/bin", "/bin"]
};

/// Points `command` (the environment's interpreter, possibly wrapped by
/// the sandbox) at the shim and sets its environment.
pub fn configure(command: &mut Command, shim: &Path, env: &LaunchEnv) {
    command
        .arg("-I")
        .arg("-B")
        .arg(shim.join(ENTRY))
        .env_clear();
    let mut path: Vec<PathBuf> = env.path.clone();
    path.extend(SYSTEM_PATH.iter().map(PathBuf::from));
    if cfg!(windows)
        && let Some(root) = std::env::var_os("SYSTEMROOT")
    {
        path.push(PathBuf::from(&root).join("System32"));
        command.env("SYSTEMROOT", root);
    }
    if let Ok(joined) = std::env::join_paths(&path) {
        command.env("PATH", joined);
    }
    command
        .env("HOME", &env.home)
        .env("TMPDIR", &env.tmp)
        .env(
            "LANG",
            std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".to_owned()),
        )
        .env("PYTHONNOUSERSITE", "1");
    if cfg!(windows) {
        command
            .env("USERPROFILE", &env.home)
            .env("TEMP", &env.tmp)
            .env("TMP", &env.tmp);
    }
    for (name, value) in &env.secrets {
        command.env(name, value);
    }
    if let Some(proxy) = &env.proxy {
        for name in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "http_proxy",
            "https_proxy",
            "ALL_PROXY",
        ] {
            command.env(name, proxy);
        }
    }
}

/// What `plugin.load` needs.
#[derive(Debug, Clone)]
pub struct LoadParams<'a> {
    /// The plugin id.
    pub plugin: &'a str,
    /// The profile name.
    pub profile: &'a str,
    /// The plugin directory.
    pub dir: &'a Path,
    /// Its data directory.
    pub data_dir: &'a Path,
    /// Granted secret names.
    pub secrets: Vec<String>,
    /// Granted capabilities (as written).
    pub capabilities: Vec<String>,
    /// The manifest facts the shim exposes as `ctx.manifest`.
    pub manifest: Value,
}

impl LoadParams<'_> {
    /// The `plugin.load` parameters, with the API table and hook names.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut api = pmt_plugins::python::api::as_json();
        api["hooks"] = json!(pmt_core::hooks::HOOK_NAMES);
        json!({
            "plugin_id": self.plugin,
            "profile": self.profile,
            "dir": self.dir,
            "data_dir": self.data_dir,
            "secrets": self.secrets,
            "capabilities": self.capabilities,
            "manifest": self.manifest,
            "api": api,
        })
    }
}
