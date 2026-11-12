//! Instantiating one plugin and reading what it registers, bounded by
//! `plugins.load_timeout_ms` (spec: plugin-system, Load order,
//! dependencies and deadline). The work runs on a thread of its own so
//! the loader can stop waiting: an overrunning plugin is skipped with
//! `load timed out` while the others load (its thread finishes in the
//! background, bounded by the plugin call timeout, and is discarded).

use std::sync::Arc;
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::Duration;

use pmt_config::grants::GrantsFile;
use pmt_core::capability::CapabilitySet;
use pmt_core::error::{Error, codes};

use super::LoadedPlugin;
use crate::grants::{Identity, PluginGrants};
use crate::host::extensions_world::Command;
use crate::host::ports::Ports;
use crate::host::provider_world::ProviderSpec;
use crate::host::state::StateDir;
use crate::host::surfaces_world::CliCommand;
use crate::host::{CallInfo, Context, Instance, Limits, Runtime};
use crate::manifest::Manifest;

/// What a plugin registers, read once at load.
#[derive(Debug, Default)]
pub(super) struct Registration {
    pub hooks: Vec<String>,
    pub sections: Vec<String>,
    pub commands: Vec<Command>,
    pub providers: Vec<ProviderSpec>,
    /// Hooks registered with `hold_stream = true` (hook world 1.2).
    pub held: Vec<String>,
    /// `prometheus plugins run` commands (hook world 1.2).
    pub cli: Vec<CliCommand>,
    /// Approval transports (hook world 1.2).
    pub transports: Vec<String>,
    /// Model-provider descriptors (provider world 1.1).
    pub models: Vec<String>,
}

/// One plugin to instantiate, owned so it can move to the load thread.
pub(super) struct Job {
    pub runtime: Runtime,
    pub ports: Ports,
    pub profile: String,
    pub profile_dir: std::path::PathBuf,
    pub limits: Limits,
    pub manifest: Manifest,
    pub identity: Identity,
    pub requested: CapabilitySet,
    pub component: Vec<u8>,
    pub dir: Option<std::path::PathBuf>,
}

/// Runs `job` within `deadline`.
pub(super) fn run(job: Job, deadline: Duration) -> Result<(LoadedPlugin, Registration), Error> {
    let id = job.manifest.id.clone();
    let (sender, receiver) = channel();
    std::thread::Builder::new()
        .name(format!("pmt-plugin-load-{id}"))
        .spawn(move || {
            let _ = sender.send(prepare(job));
        })
        .map_err(|e| {
            Error::new(
                &codes::PLUGINS_LOAD,
                format!("cannot start loading plugin '{id}': {e}"),
            )
        })?;
    match receiver.recv_timeout(deadline) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(Error::new(
            &codes::PLUGINS_LOAD,
            format!(
                "plugin '{id}': load timed out after {} ms (plugins.load_timeout_ms)",
                deadline.as_millis()
            ),
        )),
        Err(RecvTimeoutError::Disconnected) => Err(Error::new(
            &codes::PLUGINS_LOAD,
            format!("plugin '{id}': loading stopped unexpectedly"),
        )),
    }
}

fn prepare(job: Job) -> Result<(LoadedPlugin, Registration), Error> {
    let manifest = job.manifest;
    let component = job.runtime.compile(&manifest.id, &job.component)?;
    let grants = Arc::new(PluginGrants::new(
        &job.identity,
        job.requested,
        GrantsFile::at(job.profile_dir.join("grants.toml")),
    ));
    let ctx = Arc::new(Context {
        plugin: manifest.id.clone(),
        profile: job.profile,
        state: StateDir::new(&job.profile_dir, &manifest.id),
        grants: Arc::clone(&grants),
        ports: job.ports,
    });
    let instance = Instance::new(&job.runtime, &component, ctx, job.limits)?;
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
    check_adapter_operations(&job.runtime, &component, &manifest)?;
    let registration = read_registration(&instance)?;
    Ok((
        LoadedPlugin {
            manifest,
            identity: job.identity,
            instance: Arc::new(instance),
            grants,
            dir: job.dir,
        },
        registration,
    ))
}

/// A `platform-adapter` component must export every operation of the
/// adapter contract (spec: platform-adapters, Adapter contract: a plugin
/// missing a required export is refused, naming it).
fn check_adapter_operations(
    runtime: &crate::host::Runtime,
    component: &wasmtime::component::Component,
    manifest: &Manifest,
) -> Result<(), Error> {
    if manifest.adapter_platform().is_none() {
        return Ok(());
    }
    let missing = runtime.missing_operations(
        component,
        "platform-adapter",
        crate::host::ADAPTER_OPERATIONS,
    );
    match missing.as_slice() {
        [] => Ok(()),
        ops => Err(Error::new(
            &codes::PLUGINS_LOAD,
            format!(
                "plugin '{}' is a platform adapter but does not export the required operation{} {}",
                manifest.id,
                if ops.len() == 1 { "" } else { "s" },
                ops.iter()
                    .map(|op| format!("'{op}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

fn read_registration(instance: &Instance) -> Result<Registration, Error> {
    let call = CallInfo::default();
    let mut registration = Registration::default();
    if instance.implements("hook") {
        registration.hooks = instance.hooks(&call)?;
    }
    if instance.extends() {
        registration.sections = instance.sections(&call)?;
        registration.commands = instance.commands(&call)?;
    }
    if instance.has_surfaces() {
        registration.held = instance.hold_stream(&call)?;
        registration.cli = instance.cli_commands(&call)?;
        registration.transports = instance.transports(&call)?;
    }
    if instance.implements("provider") {
        registration.providers = instance.providers(&call)?;
        registration.models = instance.model_descriptors(&call)?;
    }
    Ok(registration)
}
