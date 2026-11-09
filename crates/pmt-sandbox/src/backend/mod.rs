//! The typed terminal backend registry and readiness checks
//! (spec: execution-environments, Typed backend registry and selection;
//! Built-in backend names are reserved; Backend isolation descriptor;
//! Backend readiness checks; Terminal configuration defaults).
//!
//! `local`, `container`, `ssh` and `singularity` are built in; `terminal-backend` plugins
//! register through [`BackendRegistry::register_plugin`] with a validated
//! [`Descriptor`]. A built-in name can be reserved for a later milestone
//! ([`BackendRegistry::reserve`]). Selection never falls back to another
//! backend.

use std::sync::Arc;

use pmt_config::schema::{SandboxConfig, TerminalConfig};
use pmt_core::doctor::{Check, Status};
use pmt_core::error::{Error, codes};

use crate::sandbox::Sandbox;

pub mod descriptor;
pub mod remote;

pub use descriptor::{Descriptor, Isolation};
pub use remote::{ExecOutput, ExecRequest, RemoteBackend};

/// Lower-cased names no plugin may register: the built-ins and the
/// runtimes the container and singularity backends stand for.
pub const RESERVED_NAMES: &[&str] = &[
    "local",
    "container",
    "docker",
    "podman",
    "ssh",
    "singularity",
    "apptainer",
];

/// What a registry entry is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// The OS-sandboxed local host.
    Local,
    /// A hardened rootless container.
    Container,
    /// A host reached over SSH.
    Ssh,
    /// An Apptainer/Singularity instance of the profile.
    Singularity,
    /// A built-in name not available in this build, with the milestone it
    /// ships in.
    Reserved(String),
    /// A backend provided by an enabled `terminal-backend` plugin.
    Plugin,
}

/// A selectable backend.
#[derive(Debug, Clone)]
pub struct Backend {
    /// Its configured name.
    pub name: String,
    /// What it is.
    pub kind: Kind,
    /// The plugin behind a [`Kind::Plugin`] entry.
    pub plugin: Option<Arc<dyn RemoteBackend>>,
}

impl Backend {
    /// Its isolation descriptor under `config` (the container's depends on
    /// whether the workspace is mounted).
    #[must_use]
    pub fn descriptor(&self, config: &TerminalConfig) -> Descriptor {
        match (&self.kind, &self.plugin) {
            (Kind::Container, _) => Descriptor::container(
                config.container.mount_workspace,
                config.container.persistent,
            ),
            (Kind::Ssh, _) => Descriptor::ssh(),
            (Kind::Singularity, _) => Descriptor::singularity(),
            (Kind::Plugin, Some(plugin)) => plugin.descriptor().clone(),
            _ => Descriptor::local(),
        }
    }
}

/// Every backend name `terminal.backend` may select.
#[derive(Debug, Clone)]
pub struct BackendRegistry {
    entries: Vec<Backend>,
}

impl Default for BackendRegistry {
    fn default() -> Self {
        Self::builtin()
    }
}

impl BackendRegistry {
    /// `local`, `container`, `ssh` and `singularity`.
    #[must_use]
    pub fn builtin() -> Self {
        let entry = |name: &str, kind| Backend {
            name: name.to_owned(),
            kind,
            plugin: None,
        };
        Self {
            entries: vec![
                entry("local", Kind::Local),
                entry("container", Kind::Container),
                entry("ssh", Kind::Ssh),
                entry("singularity", Kind::Singularity),
            ],
        }
    }

    /// Marks the built-in `name` as shipping in `milestone`: selecting it
    /// fails with exit 69 until then.
    pub fn reserve(&mut self, name: &str, milestone: &str) {
        let kind = Kind::Reserved(milestone.to_owned());
        match self.entries.iter_mut().find(|e| e.name == name) {
            Some(entry) => entry.kind = kind,
            None => self.entries.push(Backend {
                name: name.to_owned(),
                kind,
                plugin: None,
            }),
        }
    }

    /// Adds a plugin backend (its descriptor was validated when it was
    /// read).
    ///
    /// # Errors
    ///
    /// `terminal.backend_reserved` for a built-in name (any case);
    /// `registry.duplicate_name` when another plugin took the name.
    pub fn register_plugin(&mut self, backend: Arc<dyn RemoteBackend>) -> Result<(), Error> {
        let name = backend.name().to_owned();
        if RESERVED_NAMES.contains(&name.to_ascii_lowercase().as_str()) {
            return Err(Error::new(
                &codes::TERMINAL_BACKEND_RESERVED,
                format!("terminal backend name '{name}' is reserved"),
            ));
        }
        if self.entries.iter().any(|e| e.name == name) {
            return Err(Error::new(
                &codes::REGISTRY_DUPLICATE_NAME,
                format!("terminal backend '{name}' is already registered"),
            ));
        }
        self.entries.push(Backend {
            name,
            kind: Kind::Plugin,
            plugin: Some(backend),
        });
        Ok(())
    }

    /// Every name, built-ins first.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name.as_str()).collect()
    }

    /// The backend `name` selects.
    ///
    /// # Errors
    ///
    /// `terminal.unknown_backend` (exit 78) listing every available name;
    /// `terminal.backend_unavailable` (exit 69) for a reserved built-in.
    pub fn select(&self, name: &str) -> Result<&Backend, Error> {
        let Some(backend) = self.entries.iter().find(|e| e.name == name) else {
            return Err(Error::new(
                &codes::TERMINAL_UNKNOWN_BACKEND,
                format!("unknown terminal backend '{name}'; available: {}", self.names().join(", ")),
            )
            .with_hint("set terminal.backend to one of them, e.g. prometheus config set terminal.backend local"));
        };
        if let Kind::Reserved(milestone) = &backend.kind {
            return Err(Error::new(
                &codes::TERMINAL_BACKEND_UNAVAILABLE,
                format!("terminal backend '{name}' is available in {milestone}"),
            )
            .with_hint("set terminal.backend = \"local\""));
        }
        Ok(backend)
    }

    /// Readiness of the backend `config` selects: the `terminal` tool's
    /// availability probe and `prometheus doctor terminal` (exit
    /// [`Readiness::exit_code`]). Settings of unselected backends are not
    /// consulted. `sandbox` is the session sandbox the local backend uses;
    /// `security` decides whether a rootful container runtime is allowed.
    #[must_use]
    pub fn readiness(
        &self,
        config: &TerminalConfig,
        sandbox: &Sandbox,
        security: &SandboxConfig,
    ) -> Readiness {
        let check = match self.select(&config.backend) {
            Err(err) => fail(err.message().to_owned()),
            Ok(backend) => match &backend.kind {
                Kind::Local => local_check(sandbox),
                Kind::Container => container_check(config, security),
                Kind::Ssh => ssh_check(config),
                Kind::Singularity => singularity_check(config),
                Kind::Plugin => plugin_check(backend),
                Kind::Reserved(_) => fail("reserved backend".to_owned()),
            },
        };
        Readiness {
            checks: vec![check],
        }
    }
}

fn container_check(config: &TerminalConfig, security: &SandboxConfig) -> Check {
    match crate::container::Runtime::detect(&config.container.runtime)
        .and_then(|runtime| runtime.probe(security).map(|p| (runtime, p)))
    {
        Ok((runtime, probe)) => {
            let status = if probe.rootless {
                Status::Ok
            } else {
                Status::Warn
            };
            Check::new(
                "terminal",
                "backend",
                status,
                format!(
                    "container backend ready; runtime: {} ({})",
                    runtime.name(),
                    probe.describe()
                ),
            )
        }
        Err(err) => fail(err.message().to_owned()),
    }
}

fn ssh_check(config: &TerminalConfig) -> Check {
    if !ssh_configured(config) {
        return fail("ssh backend requires terminal.ssh.host and terminal.ssh.user");
    }
    let target = format!(
        "{}@{}:{}",
        config.ssh.user.as_deref().unwrap_or_default(),
        config.ssh.host.as_deref().unwrap_or_default(),
        config.ssh.port
    );
    Check::new(
        "terminal",
        "backend",
        Status::Ok,
        format!("ssh backend configured: {target}"),
    )
}

fn singularity_check(config: &TerminalConfig) -> Check {
    if config.singularity.image.is_empty() {
        return fail(
            "singularity backend requires terminal.singularity.image (a SIF path or docker:// reference)",
        );
    }
    match crate::singularity::Runtime::detect() {
        Ok(runtime) => Check::new(
            "terminal",
            "backend",
            Status::Ok,
            format!(
                "singularity backend ready; runtime: {}, image: {}",
                runtime.name(),
                config.singularity.image
            ),
        ),
        Err(err) => fail(err.message().to_owned()),
    }
}

fn plugin_check(backend: &Backend) -> Check {
    let Some(plugin) = &backend.plugin else {
        return fail(format!("terminal backend '{}' has no plugin", backend.name));
    };
    match plugin.check() {
        Ok(detail) => Check::new(
            "terminal",
            "backend",
            Status::Ok,
            format!("{} backend ready; {detail}", backend.name),
        ),
        Err(why) => fail(format!("{} backend unavailable: {why}", backend.name)),
    }
}

fn ssh_configured(config: &TerminalConfig) -> bool {
    config.ssh.host.as_deref().is_some_and(|h| !h.is_empty())
        && config.ssh.user.as_deref().is_some_and(|u| !u.is_empty())
}

fn fail(message: impl Into<String>) -> Check {
    Check::new("terminal", "backend", Status::Fail, message)
}

fn local_check(sandbox: &Sandbox) -> Check {
    let enforcement = sandbox.report().enforcement();
    if let Err(err) = sandbox.check() {
        return fail(err.message().to_owned());
    }
    let status = if sandbox.downgrades().is_empty() {
        Status::Ok
    } else {
        Status::Warn
    };
    Check::new(
        "terminal",
        "backend",
        status,
        format!("local backend ready; sandbox enforcement: {enforcement}"),
    )
}

/// The result of a readiness probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readiness {
    /// The `terminal` doctor checks.
    pub checks: Vec<Check>,
}

impl Readiness {
    /// Whether the backend can run commands.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.checks.iter().all(|c| c.status != Status::Fail)
    }

    /// `prometheus doctor terminal`'s exit status: 0 ready, 1 otherwise.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        u8::from(!self.is_ready())
    }

    /// The first failure's message.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.checks
            .iter()
            .find(|c| c.status == Status::Fail)
            .map(|c| c.message.as_str())
    }
}
