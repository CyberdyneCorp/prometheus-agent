//! The binary's side of the python bridge (spec: python-bridge): every
//! enabled, approved python plugin of a profile gets a supervised sidecar
//! in this process, and what it registered joins the profile's plugin set
//! with the same grant checks as a WASM plugin's.
//!
//! A plugin needs the managed interpreter and its pinned environment; an
//! environment missing at session start is built offline from the package
//! cache (resolution, which needs the network, is `plugins install` and
//! `update`). Any failure is the plugin's status; the session starts
//! without it.
//!
//! Supervisors are kept per (profile, plugin, content) for the life of the
//! process, so reloading the plugin set keeps unchanged sidecars running.

mod launcher;
pub mod migrate;
pub mod prepare;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use pmt_config::ProfileScope;
use pmt_core::capability::{Capability, PluginRight};
use pmt_core::error::Error;
use pmt_plugins::backend::Backend;
use pmt_plugins::events::{Bus, EventSink};
use pmt_plugins::host::extensions_world::Command as SlashCommand;
use pmt_plugins::host::ports::EventsPort as _;
use pmt_plugins::load::{BridgedPlugin, LoadInputs, Loaded, State};
use pmt_plugins::surfaces::builtin_names;
use pmt_pybridge::bridged::{BridgedBackend, BridgedEvents, BridgedHook, BridgedTools};
use pmt_pybridge::host::BridgeHost;
use pmt_pybridge::registrations::Registrations;
use pmt_pybridge::supervisor::Supervisor;

pub use prepare::{Prepared, prepare};

/// One hosted plugin of this process.
struct Hosted {
    /// The content key it was prepared with.
    key: String,
    supervisor: Arc<Supervisor>,
    host: Arc<BridgeHost>,
    /// Its event deliveries (the bus holds it weakly).
    sink: Arc<dyn EventSink>,
}

/// Hosted plugins of this process, by (profile dir, plugin id).
fn supervisors() -> &'static Mutex<HashMap<(PathBuf, String), Hosted>> {
    static SUPERVISORS: OnceLock<Mutex<HashMap<(PathBuf, String), Hosted>>> = OnceLock::new();
    SUPERVISORS.get_or_init(Mutex::default)
}

/// Stops every sidecar of this process (`bridge.shutdown`, then the
/// process tree; spec: Sidecar lifecycle) at exit.
pub fn shutdown_all() {
    let all: Vec<Arc<Supervisor>> = supervisors()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .drain()
        .map(|(_, hosted)| hosted.supervisor)
        .collect();
    let stops: Vec<_> = all
        .into_iter()
        .map(|s| std::thread::spawn(move || s.stop()))
        .collect();
    for stop in stops {
        let _ = stop.join();
    }
}

/// Every supervisor of `scope` in this process.
#[must_use]
pub fn running(scope: &ProfileScope) -> Vec<Arc<Supervisor>> {
    supervisors()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|((dir, _), _)| dir == scope.dir())
        .map(|(_, hosted)| Arc::clone(&hosted.supervisor))
        .collect()
}

/// The supervisor of `plugin` and its host side, reusing a running one
/// prepared with the same key.
fn supervisor(
    scope: &ProfileScope,
    plugin: &BridgedPlugin,
    prepared: Prepared,
) -> (Arc<Supervisor>, Arc<BridgeHost>, Arc<dyn EventSink>) {
    let key = (scope.dir().to_path_buf(), plugin.manifest.id.clone());
    let mut all = supervisors().lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(hosted) = all.get(&key)
        && hosted.key == prepared.key
    {
        return (
            Arc::clone(&hosted.supervisor),
            Arc::clone(&hosted.host),
            Arc::clone(&hosted.sink),
        );
    }
    let content = prepared.key.clone();
    let host = Arc::clone(&prepared.host);
    let fresh = prepared.supervisor();
    let sink: Arc<dyn EventSink> = Arc::new(BridgedEvents::new(Arc::clone(&fresh)));
    // A replaced supervisor stops its sidecar when the last session holding
    // it lets go.
    all.insert(
        key,
        Hosted {
            key: content,
            supervisor: Arc::clone(&fresh),
            host: Arc::clone(&host),
            sink: Arc::clone(&sink),
        },
    );
    (fresh, host, sink)
}

/// The profile's event bus for `loaded`, created when no WASM plugin made
/// one.
fn bus(scope: &ProfileScope, loaded: &mut Loaded) -> Arc<Bus> {
    if let Some(bus) = &loaded.bus {
        return Arc::clone(bus);
    }
    let bus = Bus::new(
        scope.name().as_str(),
        Arc::clone(&super::ports::ports(scope).events),
    );
    loaded.bus = Some(Arc::clone(&bus));
    bus
}

/// Hosts `inputs`' python plugins and adds what they register to `loaded`.
pub fn attach(scope: &ProfileScope, inputs: &LoadInputs<'_>, loaded: &mut Loaded) {
    let surveyed = pmt_plugins::load::survey(inputs);
    let bridged = pmt_plugins::load::bridged(&surveyed, inputs);
    let mut hosted: Vec<String> = Vec::new();
    for plugin in bridged {
        let id = plugin.manifest.id.clone();
        let outcome = prepare(scope, &plugin)
            .map(|prepared| supervisor(scope, &plugin, prepared))
            .and_then(|(supervisor, host, sink)| {
                let notes = register(loaded, &plugin, &supervisor)?;
                subscribe(scope, loaded, &supervisor, &host, &sink);
                Ok(notes)
            });
        let status = loaded.statuses.iter_mut().find(|s| s.id == id);
        match (outcome, status) {
            (Ok(notes), Some(status)) => {
                status.state = State::Loaded;
                status.problem = (!notes.is_empty()).then(|| notes.join("; "));
                hosted.push(id);
            }
            (Err(err), Some(status)) => {
                tracing::warn!(plugin = %id, "{}", err.message());
                status.state = State::Error;
                status.problem = Some(err.message().to_owned());
            }
            _ => {}
        }
    }
    // Plugins no longer enabled stop with their supervisor.
    supervisors()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retain(|(dir, id), _| dir != scope.dir() || hosted.contains(id));
}

/// Connects the plugin to the profile's event bus: its `ctx.emit` goes to
/// the bus, and the events it subscribed to are delivered to it.
fn subscribe(
    scope: &ProfileScope,
    loaded: &mut Loaded,
    supervisor: &Arc<Supervisor>,
    host: &Arc<BridgeHost>,
    sink: &Arc<dyn EventSink>,
) {
    let bus = bus(scope, loaded);
    host.set_events(Arc::clone(&bus) as _);
    let Some(registrations) = supervisor.registrations() else {
        return;
    };
    if registrations.subscriptions.is_empty() {
        return;
    }
    for pattern in &registrations.subscriptions {
        bus.subscribe(scope.name().as_str(), supervisor.plugin(), pattern);
    }
    bus.attach_sink(sink);
}

/// The registrations of `supervisor`'s plugin: cached, or from a start now
/// (a plugin with hooks, providers or computed sections always starts).
fn registrations(supervisor: &Supervisor) -> Result<Registrations, Error> {
    match supervisor.registrations() {
        Some(known) if !known.eager() => Ok(known),
        _ => supervisor.start_now(),
    }
}

/// Registers what the plugin provides; returns notes for its status.
fn register(
    loaded: &mut Loaded,
    plugin: &BridgedPlugin,
    supervisor: &Arc<Supervisor>,
) -> Result<Vec<String>, Error> {
    let registrations = registrations(supervisor)?;
    let granted = plugin.grants.current();
    let backend: Arc<dyn Backend> = Arc::new(BridgedBackend::new(
        Arc::clone(supervisor),
        Arc::clone(&plugin.grants),
    ));
    let mut notes: Vec<String> = registrations
        .degraded
        .iter()
        .map(|d| format!("degraded: {d}"))
        .collect();
    if let Some(unknown) = registrations
        .hooks
        .iter()
        .find(|h| pmt_core::hooks::check(h).is_err())
    {
        return Err(Error::new(
            &pmt_core::error::codes::PLUGINS_MANIFEST,
            format!("unknown hook '{unknown}'"),
        ));
    }
    let commands: Vec<SlashCommand> = registrations
        .commands
        .iter()
        .map(|c| SlashCommand {
            name: c.name.clone(),
            description: c.description.clone(),
            surfaces: Vec::new(),
        })
        .collect();
    let mut contributions = loaded.contributions.clone();
    let sections = registrations
        .sections
        .iter()
        .map(|s| s.id.clone())
        .collect();
    notes.extend(contributions.add_sections(&backend, sections, &granted)?);
    notes.extend(contributions.add_commands(&backend, commands, &granted)?);
    loaded.contributions = contributions;
    notes.extend(register_hooks(
        loaded,
        supervisor,
        &registrations.hooks,
        &granted,
    ));
    notes.extend(register_models(
        loaded,
        &backend,
        &registrations.providers,
        &granted,
    ));
    notes.extend(loaded.surfaces.add_transports(
        &backend,
        registrations.transports.clone(),
        &granted,
    ));
    notes.extend(register_objects(loaded, &backend, &registrations));
    loaded.tool_sources.push(Arc::new(BridgedTools::new(
        Arc::clone(supervisor),
        Arc::clone(&plugin.grants),
    )));
    Ok(notes)
}

/// Provider objects (web search and extract, image and video generation,
/// speech, transcription, secret sources) join the profile's provider
/// registry, which the tools of each category consult.
fn register_objects(
    loaded: &mut Loaded,
    backend: &Arc<dyn Backend>,
    registrations: &Registrations,
) -> Vec<String> {
    use pmt_plugins::providers::Category;
    let mut notes = Vec::new();
    for (kind, names) in &registrations.provider_objects {
        let category = match kind.as_str() {
            "web_search" => Category::WebSearch,
            "web_extract" => Category::WebExtract,
            "image_gen" => Category::ImageGen,
            "video_gen" => Category::VideoGen,
            "tts" => Category::Tts,
            "stt" => Category::Stt,
            "secret_source" => Category::SecretSource,
            other => {
                notes.push(format!("provider kind '{other}' is not hosted"));
                continue;
            }
        };
        for name in names {
            notes.extend(
                loaded
                    .providers
                    .add_one(backend, category, name.clone(), String::new()),
            );
        }
    }
    notes
}

fn register_hooks(
    loaded: &mut Loaded,
    supervisor: &Arc<Supervisor>,
    hooks: &[String],
    granted: &pmt_core::capability::CapabilitySet,
) -> Vec<String> {
    let mut notes = Vec::new();
    let Some(registry) = Arc::get_mut(&mut loaded.hooks) else {
        return vec!["hooks not registered: the hook set is already shared".to_owned()];
    };
    let callback = Arc::new(BridgedHook::new(Arc::clone(supervisor)));
    for hook in hooks {
        let right = Capability::Plugin {
            right: PluginRight::Hook,
            target: Some(hook.clone()),
        };
        if !granted.allows(&right) {
            notes.push(format!(
                "capability_denied: hooks {hook}; '{hook}' is not registered"
            ));
            continue;
        }
        if let Err(err) = registry.add_external(hook, Arc::clone(&callback) as _) {
            notes.push(err.message().to_owned());
        }
    }
    notes
}

fn register_models(
    loaded: &mut Loaded,
    backend: &Arc<dyn Backend>,
    profiles: &[serde_json::Value],
    granted: &pmt_core::capability::CapabilitySet,
) -> Vec<String> {
    let mut notes = Vec::new();
    let mut descriptors = Vec::new();
    for profile in profiles {
        match pmt_pybridge::provider::descriptor(profile) {
            Ok(text) => descriptors.push(text),
            Err(refusal) => notes.push(refusal),
        }
    }
    if !descriptors.is_empty() {
        notes.extend(
            loaded
                .surfaces
                .add_models(backend, &descriptors, granted, &builtin_names()),
        );
    }
    notes
}
