//! Loading a profile's plugins: discovery, enablement, approval, then one
//! WASM instance per enabled, approved `wasm` plugin, whose declared worlds
//! must match what its component exports (spec: plugin-system, Plugin
//! state and profile scope: one instance per profile, never shared).
//!
//! Every discovered plugin gets a [`Status`] (`prometheus plugins list`):
//! a manifest error, a disabled or unapproved plugin, or a failed
//! instantiation is reported there and never stops the other plugins.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pmt_config::grants::{Grants, GrantsFile};
use pmt_config::schema::PluginsConfig;
use pmt_core::capability::CapabilitySet;
use pmt_core::error::{Error, codes};
use pmt_tools::ToolRegistry;
use semver::Version;

use crate::discovery::{self, Discovered, EmbeddedPlugin, Enablement, Inputs, Source};
use crate::grants::{Approval, Identity, PluginGrants};
use crate::hooks::{HookSettings, Hooks};
use crate::host::ports::Ports;
use crate::host::state::StateDir;
use crate::host::{CallInfo, Context, Instance, Limits, Runtime};
use crate::manifest::{COMPONENT_FILE, Kind, Manifest};
use crate::tools;

/// What loading a profile's plugins needs.
#[derive(Debug, Clone, Copy)]
pub struct LoadInputs<'a> {
    /// The profile's name.
    pub profile: &'a str,
    /// The profile's home directory.
    pub profile_dir: &'a Path,
    /// The working directory (trusted project plugins).
    pub cwd: Option<&'a Path>,
    /// `[plugins]`.
    pub config: &'a PluginsConfig,
    /// Built-in plugins.
    pub embedded: &'a [EmbeddedPlugin],
    /// The running Prometheus version.
    pub running: &'a Version,
}

/// The state `prometheus plugins list` shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Instantiated.
    Loaded,
    /// Enabled but refused (see the problem).
    Error,
    /// Not enabled.
    Disabled,
}

impl State {
    /// `loaded`, `error` or `disabled`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loaded => "loaded",
            Self::Error => "error",
            Self::Disabled => "disabled",
        }
    }
}

/// One discovered plugin's outcome.
#[derive(Debug, Clone)]
pub struct Status {
    /// The plugin id.
    pub id: String,
    /// Its version, when the manifest parsed.
    pub version: Option<String>,
    /// Its kind, when the manifest parsed.
    pub kind: Option<Kind>,
    /// Where it was found.
    pub source: Source,
    /// Its digest, when its files were readable.
    pub digest: Option<String>,
    /// The outcome.
    pub state: State,
    /// Why it is not loaded, or a note (a conflict between the lists).
    pub problem: Option<String>,
    /// Lower-precedence copies it hides.
    pub shadowed: Vec<PathBuf>,
}

/// A loaded plugin.
#[derive(Debug)]
pub struct LoadedPlugin {
    /// Its manifest.
    pub manifest: Manifest,
    /// Its identity.
    pub identity: Identity,
    /// Its instance in this profile.
    pub instance: Arc<Instance>,
    /// Its live grant (re-read on change).
    pub grants: Arc<PluginGrants>,
}

/// A profile's plugins after loading.
#[derive(Debug)]
pub struct Loaded {
    /// The loaded plugins, in load order.
    pub plugins: Vec<LoadedPlugin>,
    /// Every discovered plugin's outcome, sorted by id.
    pub statuses: Vec<Status>,
    /// The hook callbacks of the loaded plugins.
    pub hooks: Arc<Hooks>,
}

impl Loaded {
    /// Registers every loaded plugin's tools into `registry` (each under
    /// its current grant); `base` is the surface and working directory of
    /// the sessions they serve. Returns the plugin toolsets that hold tools.
    pub fn register_tools(&self, registry: &mut ToolRegistry, base: &CallInfo) -> Vec<String> {
        let mut toolsets = Vec::new();
        for plugin in &self.plugins {
            if !plugin.instance.implements("tool") {
                continue;
            }
            match tools::register(registry, &plugin.instance, &plugin.grants.current(), base) {
                Ok(names) if !names.is_empty() => toolsets.push(plugin.manifest.id.clone()),
                Ok(_) => {}
                Err(err) => tracing::warn!(plugin = %plugin.manifest.id, "{}", err.message()),
            }
        }
        toolsets
    }
}

/// Reports every discovered plugin without instantiating anything.
#[must_use]
pub fn survey(inputs: &LoadInputs<'_>) -> Vec<(Discovered, Status)> {
    let grants = GrantsFile::at(inputs.profile_dir.join("grants.toml"))
        .load()
        .unwrap_or_default();
    discovery::discover(&discovery_inputs(inputs))
        .into_iter()
        .map(|d| {
            let status = status_of(&d, inputs, &grants);
            (d, status)
        })
        .collect()
}

/// Discovers and loads a profile's enabled, approved `wasm` plugins and
/// registers their hooks; a plugin subscribing to an unknown hook fails to
/// load.
#[must_use]
pub fn load(runtime: &Runtime, ports: &Ports, inputs: &LoadInputs<'_>) -> Loaded {
    let mut hooks = Hooks::new(HookSettings {
        profile_dir: inputs.profile_dir.to_path_buf(),
        timeout: Duration::from_millis(inputs.config.hook_timeout_ms),
        context_max_chars: usize::try_from(inputs.config.hook_context_max_chars)
            .unwrap_or(usize::MAX),
    });
    let mut plugins = Vec::new();
    let mut statuses = Vec::new();
    for (plugin, mut status) in survey(inputs) {
        if status.state == State::Loaded {
            let outcome = instantiate(runtime, ports, inputs, &plugin)
                .and_then(|done| subscribe(&mut hooks, done));
            match outcome {
                Ok(done) => plugins.push(done),
                Err(err) => {
                    tracing::warn!(plugin = %plugin.id, profile = inputs.profile, "{}", err.message());
                    status.state = State::Error;
                    status.problem = Some(err.message().to_owned());
                }
            }
        }
        statuses.push(status);
    }
    Loaded {
        plugins,
        statuses,
        hooks: Arc::new(hooks),
    }
}

fn subscribe(hooks: &mut Hooks, plugin: LoadedPlugin) -> Result<LoadedPlugin, Error> {
    if plugin.instance.implements("hook") {
        hooks.register(
            &plugin.instance,
            &plugin.grants.current(),
            &CallInfo::default(),
        )?;
    }
    Ok(plugin)
}

fn discovery_inputs<'a>(inputs: &LoadInputs<'a>) -> Inputs<'a> {
    Inputs {
        profile_dir: inputs.profile_dir,
        cwd: inputs.cwd,
        config: inputs.config,
        embedded: inputs.embedded,
        running: inputs.running,
    }
}

/// The status before instantiation: `Loaded` means "to instantiate".
fn status_of(plugin: &Discovered, inputs: &LoadInputs<'_>, grants: &Grants) -> Status {
    let manifest = plugin.manifest.as_ref().ok();
    let mut status = Status {
        id: plugin.id.clone(),
        version: manifest.map(|m| m.version.to_string()),
        kind: manifest.map(|m| m.kind),
        source: plugin.source.clone(),
        digest: plugin.digest.clone(),
        state: State::Disabled,
        problem: None,
        shadowed: plugin.shadowed.clone(),
    };
    let enablement = Enablement::of(plugin, inputs.config);
    let (state, problem) = match (&plugin.manifest, enablement) {
        (Err(err), _) => (State::Error, Some(err.message().to_owned())),
        (Ok(_), Enablement::Disabled) => (State::Disabled, None),
        (Ok(_), Enablement::Conflict) => {
            (State::Disabled, Some(Enablement::CONFLICT_NOTE.to_owned()))
        }
        (Ok(manifest), Enablement::Enabled) => runnable(plugin, manifest, inputs, grants),
    };
    status.state = state;
    status.problem = problem;
    status
}

fn runnable(
    plugin: &Discovered,
    manifest: &Manifest,
    inputs: &LoadInputs<'_>,
    grants: &Grants,
) -> (State, Option<String>) {
    if manifest.kind != Kind::Wasm {
        let host = match manifest.kind {
            Kind::Mcp => "the MCP client",
            _ => "the python bridge",
        };
        return (
            State::Error,
            Some(format!(
                "{} plugins are hosted by {host}, which is not available yet",
                manifest.kind.as_str()
            )),
        );
    }
    let Some(identity) = identity(plugin) else {
        return (State::Error, Some("plugin files unreadable".to_owned()));
    };
    let requested = match requested(manifest, inputs) {
        Ok(r) => r,
        Err(err) => return (State::Error, Some(err.message().to_owned())),
    };
    match Approval::of(grants, &identity, &requested).problem(&manifest.id) {
        Some(problem) => (State::Error, Some(problem)),
        None => (State::Loaded, None),
    }
}

fn identity(plugin: &Discovered) -> Option<Identity> {
    Some(Identity {
        id: plugin.manifest.as_ref().ok()?.id.clone(),
        digest: plugin.digest.clone()?,
    })
}

fn requested(manifest: &Manifest, inputs: &LoadInputs<'_>) -> Result<CapabilitySet, Error> {
    let data = StateDir::new(inputs.profile_dir, &manifest.id);
    manifest.capabilities.requested(data.root())
}

fn instantiate(
    runtime: &Runtime,
    ports: &Ports,
    inputs: &LoadInputs<'_>,
    plugin: &Discovered,
) -> Result<LoadedPlugin, Error> {
    let manifest = plugin
        .manifest
        .as_ref()
        .map_err(|e| Error::new(&codes::PLUGINS_LOAD, e.message().to_owned()))?
        .clone();
    let identity = identity(plugin)
        .ok_or_else(|| Error::new(&codes::PLUGINS_LOAD, "plugin files unreadable"))?;
    let bytes = component_bytes(plugin)?;
    let component = runtime.compile(&manifest.id, &bytes)?;
    let grants = Arc::new(PluginGrants::new(
        &identity,
        requested(&manifest, inputs)?,
        GrantsFile::at(inputs.profile_dir.join("grants.toml")),
    ));
    let ctx = Arc::new(Context {
        plugin: manifest.id.clone(),
        profile: inputs.profile.to_owned(),
        state: StateDir::new(inputs.profile_dir, &manifest.id),
        grants: Arc::clone(&grants),
        ports: ports.clone(),
    });
    let limits = Limits::from_config(&inputs.config.wasm);
    let instance = Instance::new(runtime, &component, ctx, limits)?;
    for (world, _) in &manifest.worlds {
        if !instance.implements(&world.name) {
            return Err(Error::new(
                &codes::PLUGINS_LOAD,
                format!(
                    "plugin '{}' declares world '{world}' but its component does not export prometheus:{}/plugin",
                    manifest.id, world.name
                ),
            ));
        }
    }
    Ok(LoadedPlugin {
        manifest,
        identity,
        instance: Arc::new(instance),
        grants,
    })
}

fn component_bytes(plugin: &Discovered) -> Result<Vec<u8>, Error> {
    if let Some(embedded) = plugin.embedded {
        return Ok(embedded.component.to_vec());
    }
    let dir = plugin
        .dir
        .as_ref()
        .ok_or_else(|| Error::new(&codes::PLUGINS_LOAD, "plugin has no directory"))?;
    let path = dir.join(COMPONENT_FILE);
    std::fs::read(&path).map_err(|e| Error::io(&codes::PLUGINS_LOAD, &path, &e))
}
