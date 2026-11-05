//! The contract a `terminal-backend` plugin fulfils (spec:
//! execution-environments, Remote sandboxes are plugins).
//!
//! The host forwards `execute`, `upload`, `download`, `cancel` and
//! `teardown` to the plugin; [`ExecOutput`] keeps `exit_code` optional so a
//! plugin that omits it is a contract violation the terminal reports, never
//! a silent exit 0. Implemented by `pmt-plugins` over a WASM instance.

use super::Descriptor;

/// One command for a remote backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRequest {
    /// The id `cancel` names.
    pub id: String,
    /// The shell command line.
    pub command: String,
    /// The directory it starts in (the backend's namespace).
    pub cwd: String,
    /// Its environment (no secret the session was not granted).
    pub env: Vec<(String, String)>,
    /// Milliseconds before the backend stops it; `u64::MAX` means no limit
    /// (a background command without a timeout, stopped by `cancel`).
    pub timeout_ms: u64,
}

/// What a remote backend reports for a command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOutput {
    /// The exit code; `None` is a contract violation.
    pub exit_code: Option<i32>,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
    /// The directory the command finished in, when the backend tracks it.
    pub cwd_observed: Option<String>,
}

/// A backend provided by a plugin. Calls block (a WASM call); the terminal
/// runs them off the async runtime.
pub trait RemoteBackend: Send + Sync + std::fmt::Debug {
    /// The backend name `terminal.backend` selects.
    fn name(&self) -> &str;
    /// Its validated isolation descriptor.
    fn descriptor(&self) -> &Descriptor;
    /// Readiness: `Ok(detail)` or the reason it cannot run.
    ///
    /// # Errors
    ///
    /// Why the backend is unavailable.
    fn check(&self) -> Result<String, String>;
    /// Runs one command.
    ///
    /// # Errors
    ///
    /// A backend or transport failure (a degraded result).
    fn execute(&self, request: &ExecRequest) -> Result<ExecOutput, String>;
    /// Writes `bytes` to `path` in the backend.
    ///
    /// # Errors
    ///
    /// As [`RemoteBackend::execute`].
    fn upload(&self, path: &str, bytes: &[u8]) -> Result<(), String>;
    /// Reads `path` from the backend.
    ///
    /// # Errors
    ///
    /// As [`RemoteBackend::execute`].
    fn download(&self, path: &str) -> Result<Vec<u8>, String>;
    /// Stops the command `id`.
    fn cancel(&self, id: &str);
    /// Releases the environment (session end, idle teardown).
    fn teardown(&self);
}
