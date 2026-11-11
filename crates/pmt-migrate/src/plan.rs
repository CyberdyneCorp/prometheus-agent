//! What a component reads (the [`Source`]) and writes to (the
//! [`Dest`]), and the [`Step`] each component plans: its report entries,
//! then (unless a dry run) the writes. Planning reads only the Hermes
//! profile; applying writes only the Prometheus profile.

use std::path::PathBuf;

use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_store::Store;

use crate::component::Component;
use crate::env::EnvEntry;
use crate::host::Host;
use crate::report::Entry;
use crate::source::SourceProfile;
use crate::yaml::Doc;

/// The Hermes side of one profile, read once and shared by the
/// components.
pub struct Source<'a> {
    /// The profile directory.
    pub profile: &'a SourceProfile,
    /// Its `config.yaml` as Hermes read it (empty when missing).
    pub config: &'a Doc,
    /// Its `.env`.
    pub env: &'a [EnvEntry],
    /// The binary's registries.
    pub host: &'a dyn Host,
    /// `--enable-cron`.
    pub enable_cron: bool,
    /// Where copies of Hermes databases are staged.
    pub staging: &'a std::path::Path,
}

impl Source<'_> {
    /// The config value at dotted `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        let mut node = &self.config.value;
        for segment in key.split('.') {
            node = node.get(segment)?;
        }
        Some(node)
    }

    /// The `.env` value of `name`.
    #[must_use]
    pub fn env_value(&self, name: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.value.as_str())
    }
}

/// The Prometheus side of one profile.
pub struct Dest<'a> {
    /// The profile.
    pub scope: &'a ProfileScope,
    /// Its store.
    pub store: &'a Store,
    /// `--overwrite`.
    pub overwrite: bool,
    /// The binary's side.
    pub host: &'a dyn Host,
    /// `<profile>/cache/migrate-<run-id>/` (removed when the run ends).
    pub staging: PathBuf,
}

/// One component's plan.
pub trait Step {
    /// The component.
    fn component(&self) -> Component;

    /// Its report entries (updated by [`Step::apply`]).
    fn entries(&mut self) -> &mut Vec<Entry>;

    /// Notices for the summary.
    fn warnings(&self) -> Vec<String> {
        Vec::new()
    }

    /// Dry run: marks the conflicts with the existing Prometheus profile
    /// at `profile_dir`, reading it only.
    fn predict(&mut self, _profile_dir: &std::path::Path, _overwrite: bool) {}

    /// Writes the plan.
    ///
    /// # Errors
    ///
    /// Why the component could not be committed.
    fn apply(&mut self, dest: &Dest<'_>) -> Result<(), Error>;
}
