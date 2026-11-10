//! Loading a profile's plugins: discovery, enablement, approval, then one
//! WASM instance per enabled, approved `wasm` plugin, whose declared worlds
//! must match what its component exports (spec: plugin-system, Plugin
//! state and profile scope: one instance per profile, never shared; Load
//! order, dependencies and deadline; Safe mode).
//!
//! Plugins load in `requires_plugins` order ([`order`]), each bounded by
//! `plugins.load_timeout_ms` ([`prepare`]); what they register (hooks,
//! system-prompt sections, slash commands, providers, event
//! subscriptions) is collected into the [`Loaded`] set. Every discovered
//! plugin gets a [`Status`] (`prometheus plugins list`): a manifest error,
//! a disabled or unapproved plugin, or a failed instantiation is reported
//! there and never stops the other plugins. With `plugins.safe_mode` no
//! non-embedded plugin is instantiated.

pub mod order;
mod prepare;
mod status;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pmt_config::grants::GrantsFile;
use pmt_config::schema::PluginsConfig;
use pmt_core::capability::CapabilitySet;
use pmt_core::error::{Error, codes};
use pmt_tools::ToolRegistry;
use semver::Version;

use crate::discovery::{self, Discovered, EmbeddedPlugin, Inputs, Source};
use crate::events::Bus;
use crate::extensions::Contributions;
use crate::grants::{Identity, PluginGrants};
use crate::hooks::{HookSettings, Hooks};
use crate::host::ports::Ports;
use crate::host::state::StateDir;
use crate::host::{CallInfo, Instance, Limits, Runtime};
use crate::manifest::{COMPONENT_FILE, Kind, Manifest};
use crate::providers::ProviderRegistry;
use crate::surfaces::{Surfaces, builtin_names};
use crate::tools;

use self::order::Step;
use self::prepare::{Job, Registration};
use self::status::status_of;
pub use self::status::{MODIFIED_AFTER_INSTALL, UNADOPTED_NOTE};

/// The problem a safe-mode survey reports for a plugin it skips.
pub const SAFE_MODE_NOTE: &str = "safe mode: not loaded";

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
    /// The catalog's kill list (embedded and cached live).
    pub blocked: &'a [crate::catalog::Removed],
    /// The profile's configuration, whose surface-only secrets are
    /// withheld from every plugin but their owning adapter (`None`:
    /// nothing withheld).
    pub profile_config: Option<&'a pmt_config::Config>,
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
    /// Placed in the profile by hand, without an install record: never
    /// loads until `prometheus plugins adopt`.
    Unadopted,
}

impl State {
    /// `loaded`, `error`, `disabled` or `unadopted`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loaded => "loaded",
            Self::Error => "error",
            Self::Disabled => "disabled",
            Self::Unadopted => "unadopted",
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
    /// Its directory (`None` when embedded).
    pub dir: Option<PathBuf>,
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
    /// Their system-prompt sections and slash commands.
    pub contributions: Contributions,
    /// Their provider objects.
    pub providers: ProviderRegistry,
    /// Their `plugins run` commands, approval transports and model
    /// providers.
    pub surfaces: Surfaces,
    /// Their event bus (`None` when nothing was loaded).
    pub bus: Option<Arc<Bus>>,
    /// Tools of plugins hosted outside this process (python bridge).
    pub tool_sources: Vec<Arc<dyn crate::backend::ToolSource>>,
    /// Memory providers of plugins hosted outside this process.
    pub memory_providers: Vec<crate::memory::ExternalMemory>,
}

impl Loaded {
    /// A load with nothing instantiated: `statuses` only.
    #[must_use]
    pub fn empty(statuses: Vec<Status>, settings: HookSettings) -> Self {
        Self {
            plugins: Vec::new(),
            statuses,
            hooks: Arc::new(Hooks::new(settings)),
            contributions: Contributions::default(),
            providers: ProviderRegistry::default(),
            surfaces: Surfaces::default(),
            bus: None,
            tool_sources: Vec::new(),
            memory_providers: Vec::new(),
        }
    }

    /// The skill directories of the loaded plugins as `(id, <dir>/skills)`
    /// pairs (spec: plugin-system, Plugin-bundled skills): read in
    /// place, never copied into the profile.
    #[must_use]
    pub fn skill_dirs(&self) -> Vec<(String, PathBuf)> {
        self.plugins
            .iter()
            .filter_map(|p| {
                let dir = p.dir.as_ref()?.join("skills");
                dir.is_dir().then(|| (p.manifest.id.clone(), dir))
            })
            .collect()
    }

    /// Registers every loaded plugin's tools into `registry` (each under
    /// its current grant); `base` is what listing them reports as the
    /// session, and `calls` gives each tool call's session facts (surface
    /// and working directory of the calling session). Returns the plugin
    /// toolsets that hold tools.
    pub fn register_tools(
        &self,
        registry: &mut ToolRegistry,
        base: &CallInfo,
        calls: &tools::SessionCalls,
    ) -> Vec<String> {
        let mut toolsets = Vec::new();
        for plugin in &self.plugins {
            if !plugin.instance.implements("tool") {
                continue;
            }
            match tools::register(
                registry,
                &plugin.instance,
                &plugin.grants.current(),
                base,
                calls,
            ) {
                Ok(names) if !names.is_empty() => toolsets.push(plugin.manifest.id.clone()),
                Ok(_) => {}
                Err(err) => tracing::warn!(plugin = %plugin.manifest.id, "{}", err.message()),
            }
        }
        for source in &self.tool_sources {
            if !source.register_tools(registry, base, calls).is_empty() {
                toolsets.push(source.plugin().to_owned());
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

/// Discovers and loads a profile's enabled, approved `wasm` plugins in
/// dependency order and collects what they register; a plugin subscribing
/// to an unknown hook fails to load.
#[must_use]
pub fn load(runtime: &Runtime, ports: &Ports, inputs: &LoadInputs<'_>) -> Loaded {
    let settings = HookSettings {
        profile_dir: inputs.profile_dir.to_path_buf(),
        timeout: Duration::from_millis(inputs.config.hook_timeout_ms),
        context_max_chars: usize::try_from(inputs.config.hook_context_max_chars)
            .unwrap_or(usize::MAX),
    };
    let bus = Bus::new(inputs.profile, Arc::clone(&ports.events));
    let mut ports = ports.clone();
    ports.events = Arc::clone(&bus) as _;
    let mut loader = Loader {
        loaded: Loaded::empty(Vec::new(), settings),
        hooks: None,
    };
    let mut surveyed: BTreeMap<String, (Discovered, Status)> = survey(inputs)
        .into_iter()
        .map(|(d, s)| (s.id.clone(), (d, s)))
        .collect();
    // Python plugins are hosted by the bridge (see [`bridged`]).
    let wanted: BTreeMap<String, Vec<String>> = surveyed
        .iter()
        .filter(|(_, (_, status))| {
            status.state == State::Loaded && status.kind != Some(Kind::Python)
        })
        .map(|(id, (d, _))| (id.clone(), requirements(d)))
        .collect();
    for step in order::plan(&wanted) {
        let (id, outcome) = match step {
            Step::Load(id) => {
                let (plugin, _) = &surveyed[&id];
                let outcome = loader.load_one(runtime, &ports, inputs, plugin);
                (id, outcome)
            }
            Step::Refuse(id, reason) => (id, Err(Error::new(&codes::PLUGINS_LOAD, reason))),
        };
        if let Some((_, status)) = surveyed.get_mut(&id) {
            loader.settle(status, outcome, &bus, inputs.profile);
        }
    }
    let mut done = loader.finish();
    done.statuses = surveyed.into_values().map(|(_, s)| s).collect();
    done.bus = (!done.plugins.is_empty()).then_some(bus);
    done
}

/// An enabled, approved python plugin, for the python bridge to host.
#[derive(Debug, Clone)]
pub struct BridgedPlugin {
    /// Its manifest.
    pub manifest: Manifest,
    /// Its identity.
    pub identity: Identity,
    /// Its live grant (re-read on change).
    pub grants: Arc<PluginGrants>,
    /// Its directory.
    pub dir: PathBuf,
}

/// The python plugins of a survey that are to run (state `loaded`, kind
/// `python`).
#[must_use]
pub fn bridged(surveyed: &[(Discovered, Status)], inputs: &LoadInputs<'_>) -> Vec<BridgedPlugin> {
    surveyed
        .iter()
        .filter(|(_, s)| s.state == State::Loaded && s.kind == Some(Kind::Python))
        .filter_map(|(d, _)| {
            let manifest = d.manifest.as_ref().ok()?.clone();
            let identity = identity(d)?;
            let requested = requested(&manifest, inputs).ok()?;
            let grants = Arc::new(PluginGrants::new(
                &identity,
                requested,
                GrantsFile::at(inputs.profile_dir.join("grants.toml")),
            ));
            Some(BridgedPlugin {
                manifest,
                identity,
                grants,
                dir: d.dir.clone()?,
            })
        })
        .collect()
}

/// The plugins `plugin` requires (none when its manifest did not parse).
fn requirements(plugin: &Discovered) -> Vec<String> {
    plugin
        .manifest
        .as_ref()
        .map(|m| m.requires_plugins.clone())
        .unwrap_or_default()
}

/// The load in progress.
struct Loader {
    loaded: Loaded,
    hooks: Option<Hooks>,
}

impl Loader {
    fn hooks(&mut self) -> &mut Hooks {
        let settings = self.loaded.hooks.settings().clone();
        self.hooks.get_or_insert_with(|| Hooks::new(settings))
    }

    /// Instantiates `plugin` and registers what it contributes; returns
    /// the warnings of contributions it was not granted.
    fn load_one(
        &mut self,
        runtime: &Runtime,
        ports: &Ports,
        inputs: &LoadInputs<'_>,
        plugin: &Discovered,
    ) -> Result<Vec<String>, Error> {
        // The plan places dependencies first; one that failed to load
        // refuses its dependents here.
        if let Some(dependency) = requirements(plugin)
            .into_iter()
            .find(|dep| !self.loaded.plugins.iter().any(|p| &p.manifest.id == dep))
        {
            return Err(Error::new(
                &codes::PLUGINS_LOAD,
                format!("missing dependency '{dependency}'"),
            ));
        }
        let job = job(runtime, ports, inputs, plugin)?;
        let deadline = Duration::from_millis(inputs.config.load_timeout_ms);
        let (done, registration) = prepare::run(job, deadline)?;
        let warnings = self.register(&done, registration)?;
        self.loaded.plugins.push(done);
        Ok(warnings)
    }

    fn register(
        &mut self,
        plugin: &LoadedPlugin,
        registration: Registration,
    ) -> Result<Vec<String>, Error> {
        let granted = plugin.grants.current();
        let instance = &plugin.instance;
        let backend: Arc<dyn crate::backend::Backend> = Arc::clone(instance) as _;
        // Checked before anything registers, so a refusal leaves no part of
        // the plugin behind.
        if let Some(unknown) = registration
            .hooks
            .iter()
            .find(|n| pmt_core::hooks::check(n).is_err())
        {
            return Err(Error::new(
                &codes::PLUGINS_MANIFEST,
                format!("unknown hook '{unknown}'"),
            ));
        }
        let mut contributions = self.loaded.contributions.clone();
        let mut warnings = contributions.add_sections(&backend, registration.sections, &granted)?;
        warnings.extend(contributions.add_commands(&backend, registration.commands, &granted)?);
        self.hooks()
            .register_names(instance, registration.hooks, &granted)?;
        warnings.extend(self.hooks().hold(instance.id(), &registration.held));
        warnings.extend(self.loaded.providers.add(&backend, registration.providers));
        let surfaces = &mut self.loaded.surfaces;
        warnings.extend(surfaces.add_cli(instance, registration.cli, &granted));
        warnings.extend(surfaces.add_transports(&backend, registration.transports, &granted));
        if !registration.models.is_empty() {
            warnings.extend(surfaces.add_models(
                &backend,
                &registration.models,
                &granted,
                &builtin_names(),
            ));
        }
        self.loaded.contributions = contributions;
        Ok(warnings)
    }

    /// Records `outcome` on `status`.
    fn settle(
        &self,
        status: &mut Status,
        outcome: Result<Vec<String>, Error>,
        bus: &Arc<Bus>,
        profile: &str,
    ) {
        match outcome {
            Ok(warnings) => {
                if let Some(plugin) = self
                    .loaded
                    .plugins
                    .iter()
                    .find(|p| p.manifest.id == status.id)
                {
                    bus.attach(&plugin.instance);
                }
                for warning in &warnings {
                    tracing::warn!(plugin = %status.id, profile, "{warning}");
                }
                status.state = State::Loaded;
                status.problem = (!warnings.is_empty()).then(|| warnings.join("; "));
            }
            Err(err) => {
                tracing::warn!(plugin = %status.id, profile, "{}", err.message());
                status.state = State::Error;
                status.problem = Some(err.message().to_owned());
            }
        }
    }

    fn finish(mut self) -> Loaded {
        if let Some(hooks) = self.hooks.take() {
            self.loaded.hooks = Arc::new(hooks);
        }
        self.loaded
    }
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

pub(crate) fn identity(plugin: &Discovered) -> Option<Identity> {
    Some(Identity {
        id: plugin.manifest.as_ref().ok()?.id.clone(),
        digest: plugin.digest.clone()?,
    })
}

pub(crate) fn requested(
    manifest: &Manifest,
    inputs: &LoadInputs<'_>,
) -> Result<CapabilitySet, Error> {
    let data = StateDir::new(inputs.profile_dir, &manifest.id);
    let requested = manifest.capabilities.requested(data.root())?;
    Ok(match inputs.profile_config {
        Some(config) => {
            let only = pmt_config::secrets::surface::SurfaceOnly::of(config);
            crate::grants::withhold_surface_only(manifest, &only, &requested)
        }
        None => requested,
    })
}

fn job(
    runtime: &Runtime,
    ports: &Ports,
    inputs: &LoadInputs<'_>,
    plugin: &Discovered,
) -> Result<Job, Error> {
    let manifest = plugin
        .manifest
        .as_ref()
        .map_err(|e| Error::new(&codes::PLUGINS_LOAD, e.message().to_owned()))?
        .clone();
    let identity = identity(plugin)
        .ok_or_else(|| Error::new(&codes::PLUGINS_LOAD, "plugin files unreadable"))?;
    Ok(Job {
        runtime: runtime.clone(),
        ports: ports.clone(),
        profile: inputs.profile.to_owned(),
        profile_dir: inputs.profile_dir.to_path_buf(),
        limits: Limits::from_config(&inputs.config.wasm),
        requested: requested(&manifest, inputs)?,
        component: component_bytes(plugin)?,
        dir: plugin.dir.clone(),
        manifest,
        identity,
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
