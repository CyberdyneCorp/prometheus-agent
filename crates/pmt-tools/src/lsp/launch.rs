//! Starting a language server confined (spec: lsp-diagnostics, Language
//! servers run sandboxed).
//!
//! A server reads its workspace, the system toolchain, its toolchain
//! directories below the user's home and its install directory; it writes
//! only its cache directory under `<profile>/cache/lsp/`; its environment is
//! the session's child allowlist; it reaches the network only through the
//! session's egress proxy, when there is one.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::schema::SandboxConfig;
use pmt_core::capability::{Capability, CapabilitySet};
use pmt_sandbox::{Paths, Policy, Sandbox};

use super::registry::Server;
use crate::terminal::env::ChildEnv;

/// One server start.
pub struct LaunchSpec<'a> {
    /// The server.
    pub server: &'a Server,
    /// Its resolved executable.
    pub program: &'a Path,
    /// Arguments after the executable.
    pub args: &'a [String],
    /// Its workspace root (read-only).
    pub root: &'a Path,
    /// Its only writable directory.
    pub cache_dir: &'a Path,
    /// Other directories it reads (its install directory, the managed
    /// Node that runs it).
    pub reads: Vec<PathBuf>,
    /// `PATH` for the server (the managed Node first), when it differs
    /// from the session's.
    pub path_var: Option<std::ffi::OsString>,
}

/// Builds the command of a server start.
pub trait Launcher: Send + Sync {
    /// The command for `spec`; stdio is set by the caller.
    ///
    /// # Errors
    ///
    /// The confinement cannot be built.
    fn command(&self, spec: &LaunchSpec<'_>) -> Result<tokio::process::Command, String>;
}

/// Routes a policy's network through the session's egress proxy.
pub type Egress = Arc<dyn Fn(Policy) -> Policy + Send + Sync>;

/// The OS-sandboxed launcher of a session.
pub struct SandboxLauncher {
    /// `PROMETHEUS_HOME`.
    pub prometheus_home: PathBuf,
    /// The user's home (toolchain directories are below it).
    pub user_home: PathBuf,
    /// `security.sandbox`.
    pub sandbox: SandboxConfig,
    /// The session's child environment allowlist.
    pub env: ChildEnv,
    /// The session's egress proxy; `None`: no network.
    pub egress: Option<Egress>,
}

/// What every server may do besides its own directories.
const BASE: &[&str] = &[
    "fs.read:<workspace>/**",
    "fs.read:<scratch>/**",
    "fs.write:<scratch>/**",
    "fs.read:<toolchain>",
    "process.spawn:*",
];

impl SandboxLauncher {
    fn policy(&self, spec: &LaunchSpec<'_>) -> Result<Policy, String> {
        let paths = Paths {
            user_home: self.user_home.clone(),
            prometheus_home: self.prometheus_home.clone(),
            workspace: spec.root.to_path_buf(),
            scratch: vec![spec.cache_dir.to_path_buf()],
        };
        let mut capabilities = BASE
            .iter()
            .map(|c| Capability::parse(c))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.message().to_owned())?;
        let reads = reads(spec, &self.user_home);
        capabilities.extend(reads.iter().map(|dir| Capability::read_under(dir)));
        let set: CapabilitySet = capabilities.into_iter().collect();
        let policy = Policy::from_set(&set, &paths);
        Ok(match &self.egress {
            Some(through) => through(policy),
            None => policy,
        })
    }
}

/// The directories a server reads besides the base: the executable's
/// own directory, its install and runtime directories, and its toolchain
/// directories below the user's home that exist.
fn reads(spec: &LaunchSpec<'_>, user_home: &Path) -> Vec<PathBuf> {
    let mut dirs = spec.reads.clone();
    if let Some(dir) = dunce::canonicalize(spec.program)
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        dirs.push(dir);
    }
    if let Some(dir) = spec.program.parent() {
        dirs.push(dir.to_path_buf());
    }
    dirs.extend(
        spec.server
            .toolchain
            .iter()
            .map(|sub| user_home.join(sub))
            .filter(|dir| dir.exists()),
    );
    dirs.sort();
    dirs.dedup();
    dirs
}

impl Launcher for SandboxLauncher {
    fn command(&self, spec: &LaunchSpec<'_>) -> Result<tokio::process::Command, String> {
        std::fs::create_dir_all(spec.cache_dir)
            .map_err(|e| format!("cannot create {}: {e}", spec.cache_dir.display()))?;
        let sandbox =
            Sandbox::new(self.policy(spec)?, &self.sandbox).map_err(|e| e.message().to_owned())?;
        let mut command = sandbox
            .command(spec.program)
            .map_err(|e| e.message().to_owned())?;
        command.args(spec.args).env_clear();
        self.env.apply(&mut command);
        environment(&mut command, spec);
        command.current_dir(spec.root);
        Ok(tokio::process::Command::from(command))
    }
}

/// The server's own environment on top of the allowlist: its cache
/// variables (pointed into the cache directory) and `lsp.servers.<id>.env`.
pub fn environment(command: &mut std::process::Command, spec: &LaunchSpec<'_>) {
    for (name, sub) in &spec.server.cache_env {
        command.env(name, spec.cache_dir.join(sub));
    }
    command.env("TMPDIR", spec.cache_dir);
    if let Some(path) = &spec.path_var {
        command.env("PATH", path);
    }
    command.envs(&spec.server.env);
}

/// Starts servers unconfined, with this process's environment: for tests
/// of the protocol with stand-in servers.
pub struct Unconfined;

impl Launcher for Unconfined {
    fn command(&self, spec: &LaunchSpec<'_>) -> Result<tokio::process::Command, String> {
        std::fs::create_dir_all(spec.cache_dir).map_err(|e| e.to_string())?;
        let mut command = std::process::Command::new(spec.program);
        command.args(spec.args).current_dir(spec.root);
        environment(&mut command, spec);
        Ok(tokio::process::Command::from(command))
    }
}
