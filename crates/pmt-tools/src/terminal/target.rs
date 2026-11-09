//! Where a session's shell processes run (spec: execution-environments,
//! Typed backend registry and selection; Backend isolation descriptor).
//!
//! The terminal plans, guards, shapes and tracks commands the same way on
//! every backend; a [`Target`] is the part that differs: how a shell argv
//! becomes a running process and how the shell's small state files (the
//! environment snapshot, the reported cwd) are read. [`LocalTarget`] runs
//! under the OS sandbox on this host; [`super::remote::RemoteTarget`] runs
//! in a container or over SSH; [`PluginTarget`] forwards to a
//! `terminal-backend` plugin.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_sandbox::Sandbox;
use pmt_sandbox::backend::{Descriptor, RemoteBackend};

use super::env::ChildEnv;
use super::shell::Shell;
use super::spawn::{self, SpawnError, Spawned};

/// How one backend runs the session's shell processes.
pub trait Target: Send + Sync + std::fmt::Debug {
    /// The backend's isolation descriptor.
    fn descriptor(&self) -> &Descriptor;
    /// Refuses before anything runs when the backend cannot run commands
    /// (the local sandbox below its required strength).
    ///
    /// # Errors
    ///
    /// The refusal text.
    fn check(&self) -> Result<(), String>;
    /// Makes the environment reachable (starts the container, opens the
    /// SSH master). Blocks.
    ///
    /// # Errors
    ///
    /// As [`spawn::spawn`].
    fn connect(&self) -> Result<(), SpawnError>;
    /// Forgets cached connection state after a failure.
    fn evict(&self);
    /// The shell to use when the backend picks its own (remote), after
    /// [`Target::connect`].
    fn shell(&self) -> Option<Shell>;
    /// The home directory `~` expands to, after [`Target::connect`].
    fn home(&self) -> Option<PathBuf>;
    /// Where the shell's state files live, after [`Target::connect`].
    fn state_dir(&self) -> PathBuf;
    /// Starts `argv` with `env` in `cwd`.
    ///
    /// # Errors
    ///
    /// As [`spawn::spawn`].
    fn spawn(
        &self,
        env: &ChildEnv,
        argv: &[OsString],
        cwd: &Path,
        stdin: bool,
    ) -> Result<Spawned, SpawnError>;
    /// Reads and deletes a small state file.
    fn take_text(&self, path: &Path) -> Option<String>;
    /// Whether `path` is a file in the backend.
    fn is_file(&self, path: &Path) -> bool;
    /// Deletes a state file.
    fn remove(&self, path: &Path);
    /// When a foreground command exited with `code`, whether that was the
    /// backend failing rather than the command (the reason, after a
    /// health probe). Blocks.
    fn failure(&self, code: i32) -> Option<String>;
    /// Session end: releases the environment. Blocks.
    fn close(&self);
    /// The plugin a [`PluginTarget`] forwards to.
    fn plugin(&self) -> Option<Arc<dyn RemoteBackend>> {
        None
    }
}

/// The `local` backend: this host under the session's OS sandbox.
#[derive(Debug)]
pub struct LocalTarget {
    sandbox: Arc<Sandbox>,
    scratch: PathBuf,
    descriptor: Descriptor,
}

impl LocalTarget {
    /// Commands run under `sandbox`; state files go to `scratch`.
    #[must_use]
    pub fn new(sandbox: Arc<Sandbox>, scratch: &Path) -> Self {
        Self {
            sandbox,
            scratch: scratch.to_path_buf(),
            descriptor: Descriptor::local(),
        }
    }
}

impl Target for LocalTarget {
    fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    fn check(&self) -> Result<(), String> {
        self.sandbox.check().map_err(|err| err.message().to_owned())
    }

    fn connect(&self) -> Result<(), SpawnError> {
        Ok(())
    }

    fn evict(&self) {}

    fn shell(&self) -> Option<Shell> {
        None
    }

    fn home(&self) -> Option<PathBuf> {
        None
    }

    fn state_dir(&self) -> PathBuf {
        self.scratch.clone()
    }

    fn spawn(
        &self,
        env: &ChildEnv,
        argv: &[OsString],
        cwd: &Path,
        stdin: bool,
    ) -> Result<Spawned, SpawnError> {
        spawn::spawn(&self.sandbox, env, argv, cwd, stdin)
    }

    fn take_text(&self, path: &Path) -> Option<String> {
        let text = std::fs::read_to_string(path).ok();
        let _ = std::fs::remove_file(path);
        text
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn remove(&self, path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    fn failure(&self, _code: i32) -> Option<String> {
        None
    }

    fn close(&self) {}
}

/// A `terminal-backend` plugin (spec: execution-environments, Remote
/// sandboxes are plugins): commands go to its `execute`, one request per
/// foreground call; it has no host processes to spawn.
#[derive(Debug)]
pub struct PluginTarget {
    backend: Arc<dyn RemoteBackend>,
}

impl PluginTarget {
    /// The target of `backend`.
    #[must_use]
    pub fn new(backend: Arc<dyn RemoteBackend>) -> Self {
        Self { backend }
    }
}

impl Target for PluginTarget {
    fn descriptor(&self) -> &Descriptor {
        self.backend.descriptor()
    }

    fn check(&self) -> Result<(), String> {
        Ok(())
    }

    fn connect(&self) -> Result<(), SpawnError> {
        Ok(())
    }

    fn evict(&self) {}

    fn shell(&self) -> Option<Shell> {
        None
    }

    fn home(&self) -> Option<PathBuf> {
        None
    }

    fn state_dir(&self) -> PathBuf {
        PathBuf::from(&self.backend.descriptor().default_cwd)
    }

    fn spawn(
        &self,
        _env: &ChildEnv,
        _argv: &[OsString],
        _cwd: &Path,
        _stdin: bool,
    ) -> Result<Spawned, SpawnError> {
        Err(SpawnError::Infrastructure(format!(
            "terminal backend '{}' runs commands through its plugin only",
            self.backend.name()
        )))
    }

    fn take_text(&self, _path: &Path) -> Option<String> {
        None
    }

    fn is_file(&self, _path: &Path) -> bool {
        false
    }

    fn remove(&self, _path: &Path) {}

    fn failure(&self, _code: i32) -> Option<String> {
        None
    }

    fn close(&self) {
        self.backend.teardown();
    }

    fn plugin(&self) -> Option<Arc<dyn RemoteBackend>> {
        Some(Arc::clone(&self.backend))
    }
}

/// A selected backend that could not be set up (no container runtime, an
/// unconfigured SSH destination, a refused option): every call is refused
/// with the reason. Never a fallback to another backend.
#[derive(Debug)]
pub struct Unavailable {
    reason: String,
    descriptor: Descriptor,
}

impl Unavailable {
    /// Refuses with `reason`.
    #[must_use]
    pub fn new(reason: impl Into<String>, descriptor: Descriptor) -> Self {
        Self {
            reason: reason.into(),
            descriptor,
        }
    }
}

impl Target for Unavailable {
    fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    fn check(&self) -> Result<(), String> {
        Err(self.reason.clone())
    }

    fn connect(&self) -> Result<(), SpawnError> {
        Err(SpawnError::Infrastructure(self.reason.clone()))
    }

    fn evict(&self) {}

    fn shell(&self) -> Option<Shell> {
        None
    }

    fn home(&self) -> Option<PathBuf> {
        None
    }

    fn state_dir(&self) -> PathBuf {
        PathBuf::from("/")
    }

    fn spawn(
        &self,
        _env: &ChildEnv,
        _argv: &[OsString],
        _cwd: &Path,
        _stdin: bool,
    ) -> Result<Spawned, SpawnError> {
        Err(SpawnError::Infrastructure(self.reason.clone()))
    }

    fn take_text(&self, _path: &Path) -> Option<String> {
        None
    }

    fn is_file(&self, _path: &Path) -> bool {
        false
    }

    fn remove(&self, _path: &Path) {}

    fn failure(&self, _code: i32) -> Option<String> {
        None
    }

    fn close(&self) {}
}
