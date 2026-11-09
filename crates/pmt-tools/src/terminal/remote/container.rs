//! The `container` backend's transport: `podman exec` / `docker exec` into
//! the session's hardened container (spec: execution-environments,
//! Container hardening is never silently dropped; Container lifecycle and
//! orphan reaping).
//!
//! Variables reach the container as `--env NAME` with the value set on the
//! client process only, so a secret never appears on a command line.

use std::process::Command;
use std::sync::Arc;

use pmt_core::error::codes;
use pmt_sandbox::container::{Container, Flavor};

use super::{EnvVar, Transport};
use crate::terminal::spawn::SpawnError;

/// Exec into one container.
#[derive(Debug)]
pub struct ContainerTransport {
    container: Arc<Container>,
    user: std::sync::OnceLock<usize>,
}

impl ContainerTransport {
    /// The transport of `container`.
    #[must_use]
    pub fn new(container: Arc<Container>) -> Self {
        Self {
            container,
            user: std::sync::OnceLock::new(),
        }
    }

    /// Registers the session's activity with the container's idle reaper.
    pub fn attach(&self, activity: std::sync::Weak<dyn pmt_sandbox::container::Activity>) {
        let _ = self.user.set(self.container.attach(activity));
    }

    /// The container.
    #[must_use]
    pub fn container(&self) -> &Arc<Container> {
        &self.container
    }
}

impl Transport for ContainerTransport {
    fn describe(&self) -> String {
        format!("container {}", self.container.name())
    }

    fn connect(&self) -> Result<(), SpawnError> {
        self.container.ensure_running().map_err(|err| {
            if err.code() == &codes::TERMINAL_CONTAINER_HARDENING {
                SpawnError::Refused(err)
            } else {
                SpawnError::Infrastructure(err.message().to_owned())
            }
        })
    }

    fn evict(&self) {
        self.container.evict();
    }

    fn command(&self, argv: &[String], env: &[EnvVar], stdin: bool) -> Result<Command, String> {
        let mut command = self.container.runtime().command();
        command.arg("exec");
        if stdin {
            command.arg("--interactive");
        }
        for var in env {
            command
                .arg(format!("--env={}", var.name))
                .env(&var.name, &var.value);
        }
        command.arg(self.container.name()).args(argv);
        Ok(command)
    }

    fn failure_code(&self) -> i32 {
        match self.container.runtime().flavor() {
            Flavor::Podman => 125,
            Flavor::Docker => 1,
        }
    }

    fn close(&self) {
        self.container.release(self.user.get().copied());
    }
}
