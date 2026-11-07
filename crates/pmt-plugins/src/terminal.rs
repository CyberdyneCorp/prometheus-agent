//! `terminal-backend` plugins as terminal backends (spec:
//! execution-environments, Remote sandboxes are plugins; Built-in backend
//! names are reserved; Backend isolation descriptor).
//!
//! [`PluginBackend::read`] reads a loaded plugin's backend name and
//! isolation descriptor once; a missing or invalid descriptor rejects the
//! backend then, before any session can select it. Calls are forwarded to
//! the instance under its usual fuel, epoch and memory bounds; a trap or
//! timeout is a backend failure (a degraded terminal result).

use std::sync::Arc;

use pmt_core::error::Error;
use pmt_sandbox::backend::{BackendRegistry, Descriptor, ExecOutput, ExecRequest, RemoteBackend};

use crate::host::terminal_backend_world::Request;
use crate::host::{CallInfo, Instance};

/// One plugin's backend.
#[derive(Debug)]
pub struct PluginBackend {
    instance: Arc<Instance>,
    name: String,
    descriptor: Descriptor,
    call: CallInfo,
}

impl PluginBackend {
    /// The backend `instance` provides.
    ///
    /// # Errors
    ///
    /// `plugins.load` for a component without the 1.1 contract,
    /// `terminal.backend_descriptor` for a missing field or an invalid
    /// value, `plugins.call` when reading traps.
    pub fn read(instance: Arc<Instance>, call: CallInfo) -> Result<Self, Error> {
        let (name, json) = instance.backend_identity(&call)?;
        let descriptor = Descriptor::from_json(&name, &json)?;
        Ok(Self {
            instance,
            name,
            descriptor,
            call,
        })
    }
}

impl RemoteBackend for PluginBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    fn check(&self) -> Result<String, String> {
        self.instance
            .backend_check(&self.call)
            .map_err(|err| err.message().to_owned())?
    }

    fn execute(&self, request: &ExecRequest) -> Result<ExecOutput, String> {
        let request = Request {
            id: request.id.clone(),
            line: request.command.clone(),
            cwd: request.cwd.clone(),
            env: request.env.clone(),
            timeout_ms: request.timeout_ms,
        };
        let execution = self
            .instance
            .backend_execute(&self.call, &request)
            .map_err(|err| err.message().to_owned())??;
        Ok(ExecOutput {
            exit_code: execution.exit_code,
            stdout: execution.stdout,
            stderr: execution.stderr,
            cwd_observed: execution.cwd_observed,
        })
    }

    fn upload(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        self.instance
            .backend_upload(&self.call, path, bytes)
            .map_err(|err| err.message().to_owned())?
    }

    fn download(&self, path: &str) -> Result<Vec<u8>, String> {
        self.instance
            .backend_download(&self.call, path)
            .map_err(|err| err.message().to_owned())?
    }

    fn cancel(&self, id: &str) {
        if let Err(err) = self.instance.backend_cancel(&self.call, id) {
            tracing::warn!(backend = %self.name, "cancel failed: {}", err.message());
        }
    }

    fn teardown(&self) {
        if let Err(err) = self.instance.backend_teardown(&self.call) {
            tracing::warn!(backend = %self.name, "teardown failed: {}", err.message());
        }
    }
}

/// The built-in backends plus every loaded `terminal-backend` plugin's;
/// a plugin whose backend cannot be read or whose name is reserved is
/// left out with a WARN naming why, and the rejections are returned.
#[must_use]
pub fn registry(
    loaded: &crate::load::Loaded,
    call: &CallInfo,
) -> (BackendRegistry, Vec<(String, Error)>) {
    let mut registry = BackendRegistry::builtin();
    let mut rejected = Vec::new();
    for plugin in &loaded.plugins {
        let declares = plugin
            .manifest
            .worlds
            .iter()
            .any(|(world, _)| world.name == "terminal-backend");
        if !declares {
            continue;
        }
        let added = PluginBackend::read(Arc::clone(&plugin.instance), call.clone())
            .and_then(|backend| registry.register_plugin(Arc::new(backend)));
        if let Err(err) = added {
            tracing::warn!(plugin = %plugin.manifest.id, "terminal backend rejected: {}", err.message());
            rejected.push((plugin.manifest.id.clone(), err));
        }
    }
    (registry, rejected)
}
