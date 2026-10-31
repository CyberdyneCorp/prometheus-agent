//! The typed terminal backend registry and readiness checks
//! (spec: execution-environments, Typed backend registry and selection;
//! Backend readiness checks; Terminal configuration defaults).
//!
//! `local` is built in. `container` and `ssh` are reserved entries whose
//! selection fails until they ship in M2; plugin backends (M2) register
//! through [`BackendRegistry::register_plugin`]. Selection never falls back
//! to another backend.

use pmt_config::schema::TerminalConfig;
use pmt_core::doctor::{Check, Status};
use pmt_core::error::{Error, codes};

use crate::sandbox::Sandbox;

/// The milestone the reserved built-ins ship in.
const RESERVED_UNTIL: &str = "M2";

/// What a registry entry is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// The OS-sandboxed local host.
    Local,
    /// A built-in name that is not available in this build.
    Reserved,
    /// A backend provided by an enabled `terminal-backend` plugin.
    Plugin,
}

/// A selectable backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backend {
    /// Its configured name.
    pub name: String,
    /// What it is.
    pub kind: Kind,
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
    /// `local`, plus `container` and `ssh` reserved.
    #[must_use]
    pub fn builtin() -> Self {
        let entry = |name: &str, kind| Backend {
            name: name.to_owned(),
            kind,
        };
        Self {
            entries: vec![
                entry("local", Kind::Local),
                entry("container", Kind::Reserved),
                entry("ssh", Kind::Reserved),
            ],
        }
    }

    /// Adds a plugin backend.
    ///
    /// # Errors
    ///
    /// `registry.duplicate_name` when the name is taken.
    pub fn register_plugin(&mut self, name: &str) -> Result<(), Error> {
        if self.entries.iter().any(|e| e.name == name) {
            return Err(Error::new(
                &codes::REGISTRY_DUPLICATE_NAME,
                format!("terminal backend '{name}' is already registered"),
            ));
        }
        self.entries.push(Backend {
            name: name.to_owned(),
            kind: Kind::Plugin,
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
        if backend.kind == Kind::Reserved {
            return Err(Error::new(
                &codes::TERMINAL_BACKEND_UNAVAILABLE,
                format!("terminal backend '{name}' is available in {RESERVED_UNTIL}"),
            )
            .with_hint("set terminal.backend = \"local\""));
        }
        Ok(backend)
    }

    /// Readiness of the backend `config` selects: the `terminal` tool's
    /// availability probe and `prometheus doctor terminal` (exit
    /// [`Readiness::exit_code`]). Settings of unselected backends are not
    /// consulted. `sandbox` is the session sandbox the local backend uses.
    #[must_use]
    pub fn readiness(&self, config: &TerminalConfig, sandbox: &Sandbox) -> Readiness {
        let name = config.backend.as_str();
        let check = match self.entries.iter().find(|e| e.name == name) {
            Some(backend) if backend.kind == Kind::Local => local_check(sandbox),
            Some(_) if name == "ssh" && !ssh_configured(config) => {
                fail("ssh backend requires terminal.ssh.host and terminal.ssh.user")
            }
            _ => match self.select(name) {
                Ok(_) => fail(format!("terminal backend '{name}' has no readiness check")),
                Err(err) => fail(err.message().to_owned()),
            },
        };
        Readiness {
            checks: vec![check],
        }
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
