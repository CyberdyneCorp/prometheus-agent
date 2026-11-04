//! Everything one python plugin needs before its sidecar can start (spec:
//! python-bridge, Managed CPython runtime; Per-plugin pinned environments;
//! Sidecar lifecycle): the managed interpreter, its locked environment
//! (built offline when missing), the shim, the sandboxed launcher, the
//! host side of its `host.call` requests and its log.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_plugins::load::BridgedPlugin;
use pmt_plugins::python::PythonLaunch;
use pmt_pybridge::env::{self, Env};
use pmt_pybridge::host::BridgeHost;
use pmt_pybridge::launch::LoadParams;
use pmt_pybridge::log::Log;
use pmt_pybridge::runtime::{self, Interpreter, Uv};
use pmt_pybridge::shim;
use pmt_pybridge::supervisor::{PluginSpec, Settings, Supervisor};
use sha2::{Digest as _, Sha256};

use super::launcher::{Layout, Sandboxed};

/// A plugin ready to supervise.
pub struct Prepared {
    /// What its sidecar is built from (plugin content, shim, lock and
    /// interpreter); a change starts a new supervisor.
    pub key: String,
    spec: PluginSpec,
    settings: Settings,
    /// Its host side (`plugins run` attaches the terminal).
    pub host: Arc<BridgeHost>,
    /// Its environment.
    pub env: Env,
    /// Its interpreter.
    pub python: Interpreter,
}

impl Prepared {
    /// A supervisor over it (nothing started).
    #[must_use]
    pub fn supervisor(self) -> Arc<Supervisor> {
        Supervisor::new(self.spec, self.settings)
    }
}

/// `<profile>/python/locks/<id>/`.
#[must_use]
pub fn lock_dir(scope: &ProfileScope, id: &str) -> PathBuf {
    scope.dir().join("python").join("locks").join(id)
}

/// The managed interpreter and uv of `scope`'s configured version.
///
/// # Errors
///
/// `bridge.runtime_missing` with the install hint.
pub fn toolchain(scope: &ProfileScope) -> Result<(Interpreter, Uv), Error> {
    let version = scope.config().python_bridge.python_version;
    let python = runtime::require(scope.home(), version)?;
    let uv = Uv::of(scope.home()).ok_or_else(|| runtime::missing(version))?;
    Ok((python, uv))
}

/// The plugin's requirements, checked against the upper-bound rule.
///
/// # Errors
///
/// `bridge.requirements`.
pub fn requirements(
    scope: &ProfileScope,
    dir: &Path,
    launch: &PythonLaunch,
) -> Result<Vec<String>, Error> {
    let requirements = env::requirements(dir, &launch.requirements)?;
    env::check_bounds(
        &requirements,
        scope.config().python_bridge.allow_unbounded_requirements,
    )?;
    Ok(requirements)
}

/// Resolves the plugin's lock (network; `plugins install`/`update`) and
/// builds its environment.
///
/// # Errors
///
/// `bridge.*` errors.
pub fn install(
    scope: &ProfileScope,
    id: &str,
    dir: &Path,
    launch: &PythonLaunch,
) -> Result<Env, Error> {
    let (python, uv) = toolchain(scope)?;
    let wanted = requirements(scope, dir, launch)?;
    let lock = env::lock(&uv, &python, &wanted, &lock_dir(scope, id))?;
    env::ensure(&uv, &python, &envs_root(scope), id, &lock, false)
}

/// `<profile>/python/envs`.
#[must_use]
pub fn envs_root(scope: &ProfileScope) -> PathBuf {
    scope.dir().join("python").join("envs")
}

/// The lock of an installed plugin, created here only when it has no
/// requirements (that needs no network).
fn lock(
    scope: &ProfileScope,
    plugin: &BridgedPlugin,
    uv: &Uv,
    python: &Interpreter,
) -> Result<PathBuf, Error> {
    let id = &plugin.manifest.id;
    let wanted = requirements(scope, &plugin.dir, &PythonLaunch::of(&plugin.manifest))?;
    let dir = lock_dir(scope, id);
    let lock = dir.join(env::LOCK_FILE);
    let resolved = std::fs::read_to_string(dir.join("requirements.in")).unwrap_or_default();
    let current = if wanted.is_empty() {
        String::new()
    } else {
        wanted.join("\n") + "\n"
    };
    if lock.is_file() && resolved == current {
        return Ok(lock);
    }
    if wanted.is_empty() {
        return env::lock(uv, python, &wanted, &dir);
    }
    Err(Error::new(
        &codes::BRIDGE_REQUIREMENTS,
        format!("plugin '{id}' has no lock for its current requirements"),
    )
    .with_hint(format!(
        "run `prometheus plugins update {id}` with network access"
    )))
}

/// Prepares `plugin` of `scope`.
///
/// # Errors
///
/// The runtime, lock, environment or shim failure (the plugin's status).
pub fn prepare(scope: &ProfileScope, plugin: &BridgedPlugin) -> Result<Prepared, Error> {
    let id = plugin.manifest.id.clone();
    let (python, uv) = toolchain(scope)?;
    let lock = lock(scope, plugin, &uv, &python)?;
    let env = env::ensure(&uv, &python, &envs_root(scope), &id, &lock, true)?;
    let shim = shim::materialize(&scope.dir().join("python").join("shim"))?;
    let data = scope.dir().join("plugin-data").join(&id);
    let tmp = scope.dir().join("cache").join("bridge-tmp").join(&id);
    let lock_text = std::fs::read_to_string(&lock).unwrap_or_default();
    let key = key(&[
        &plugin.identity.digest,
        &shim::digest(),
        &env::lock_digest(&lock_text),
        &python.version,
    ]);
    let log = Arc::new(Log::new(Log::path_of(scope.dir(), &id)));
    let granted = plugin.grants.current();
    let load = LoadParams {
        plugin: &id,
        profile: scope.name().as_str(),
        dir: &plugin.dir,
        data_dir: &data,
        secrets: granted
            .iter()
            .filter_map(|c| match c {
                pmt_core::capability::Capability::SecretsRead(name) => Some(name.clone()),
                _ => None,
            })
            .collect(),
        capabilities: granted.iter().map(ToString::to_string).collect(),
        manifest: serde_json::json!({"name": id, "version": plugin.manifest.version.to_string()}),
    }
    .to_json();
    let launcher = Arc::new(Sandboxed {
        scope: scope.clone(),
        layout: Layout {
            plugin_dir: plugin.dir.clone(),
            env: env.clone(),
            python_dir: python.dir.clone(),
            shim,
            data,
            tmp,
        },
        grants: Arc::clone(&plugin.grants),
        proxy: Mutex::new(None),
    });
    let host = Arc::new(BridgeHost::new(
        &id,
        scope.name().as_str(),
        Arc::clone(&plugin.grants),
        super::super::ports::ports(scope),
        tokio::runtime::Handle::try_current().ok(),
        Arc::clone(&log),
        loaded_probe(scope),
    ));
    let config = scope.config();
    let spec = PluginSpec {
        id: id.clone(),
        profile: scope.name().to_string(),
        profile_dir: scope.dir().to_path_buf(),
        load,
        launcher,
        host: Arc::clone(&host) as _,
        log,
        cache: pmt_pybridge::registrations::cache_path(scope.dir(), &id, &key),
    };
    Ok(Prepared {
        key,
        spec,
        settings: Settings::from_config(&config.python_bridge, &config.plugins),
        host,
        env,
        python,
    })
}

/// `ctx.has_plugin`: another plugin of the profile is enabled.
fn loaded_probe(scope: &ProfileScope) -> Arc<dyn Fn(&str) -> bool + Send + Sync> {
    let enabled = scope.config().plugins.enabled.clone();
    Arc::new(move |id| enabled.iter().any(|e| e == id))
}

fn key(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    pmt_pybridge::hex(&hasher.finalize())
}
