//! What the migrator asks of the binary that runs it: the registries that
//! live there (toolsets, themes, adapters), the job-validation catalog,
//! the Python-bridge plugin import and progress output. Tests supply a
//! fake.

use std::path::Path;

use pmt_config::ProfileScope;
use pmt_core::error::Error;

/// A registry of Prometheus names (spec: hermes-migration, Typed names
/// validated against Prometheus registries).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registry {
    /// Toolset names.
    Toolset,
    /// Provider ids and aliases.
    Provider,
    /// Display themes.
    Theme,
    /// Messaging adapters built into the binary.
    Platform,
    /// Memory provider plugins installed in the profile.
    MemoryProvider,
}

/// One imported Hermes plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginOutcome {
    /// Its id.
    pub id: String,
    /// Runs on the Python bridge (`compatible` or `degraded`).
    pub bridge_compatible: bool,
    /// Hermes had it enabled.
    pub was_enabled: bool,
    /// The APIs that make it incompatible.
    pub blocking: Vec<String>,
    /// Capabilities proposed for approval at first enable.
    pub proposed: Vec<String>,
    /// Why it was not copied (a plugin of that id exists).
    pub skipped: Option<String>,
}

/// The binary's side of a migration.
pub trait Host {
    /// Whether `name` is registered in `registry` for `scope`'s profile.
    fn known(&self, registry: Registry, name: &str, scope: Option<&ProfileScope>) -> bool;

    /// Whether Prometheus providers, adapters or tools register `name` as
    /// a credential.
    fn known_secret(&self, name: &str) -> bool;

    /// The registries scheduled jobs of `scope` are validated against.
    fn cron_catalog(&self, scope: &ProfileScope) -> Box<dyn pmt_cron::validate::Catalog>;

    /// Copies the Hermes plugins under `dir` into `scope`'s profile,
    /// disabled and without grants; `enabled` were enabled in Hermes.
    ///
    /// # Errors
    ///
    /// When the plugins cannot be read or copied.
    fn import_plugins(
        &self,
        scope: &ProfileScope,
        dir: &Path,
        enabled: &[String],
    ) -> Result<Vec<PluginOutcome>, Error>;

    /// Prints a progress line (stderr).
    fn progress(&self, line: &str);

    /// Whether the user asked to stop (Ctrl+C): the run stops at the next
    /// commit boundary.
    fn interrupted(&self) -> bool {
        false
    }
}
