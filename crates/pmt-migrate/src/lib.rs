//! `prometheus migrate from-hermes` (spec: hermes-migration): a read-only
//! import of a Hermes Agent home into a Prometheus home.
//!
//! - [`source`] finds the Hermes home and its profiles and never writes
//!   to it; `state.db` is read through a staged copy.
//! - [`run`] selects the profiles, plans every selected [`component`] of
//!   each from the Hermes side alone ([`plan::Step`]) and, unless a dry
//!   run, commits them one by one into the Prometheus profile.
//! - The components: [`config`] (`config.yaml` through a declarative
//!   mapping table), [`secrets`] (`.env` and credential files),
//!   [`identity`], [`memory`], [`skills`], [`sessions`], [`cron`],
//!   [`mcp`], [`plugins`] and [`gateway`].
//! - [`report`] gives every Hermes item exactly one disposition; the
//!   binary prints it and a real run saves it in the profile.
//! - [`host`] is what the binary supplies: its registries, the job
//!   catalog and the Python-bridge plugin import.

pub mod agents;
pub mod commit;
pub mod component;
pub mod config;
pub mod cron;
mod data;
pub mod env;
pub mod gateway;
pub mod host;
pub mod identity;
pub mod ledger;
pub mod mcp;
pub mod memory;
pub mod plan;
pub mod plugins;
pub mod report;
pub mod run;
pub mod secrets;
pub mod sessions;
pub mod skills;
pub mod source;
#[cfg(test)]
mod testkit;
pub mod writes;
pub mod yaml;

pub use component::Component;
pub use host::{Host, PluginOutcome, Registry};
pub use report::{Disposition, Report};
pub use run::{Options, run};

/// The default Hermes home: `~/.hermes` on POSIX, `%LOCALAPPDATA%\hermes`
/// on Windows (Hermes environment variables such as `HERMES_HOME` are
/// never read).
#[must_use]
pub fn default_hermes_home(
    user_home: &std::path::Path,
    local_app_data: Option<&std::path::Path>,
) -> std::path::PathBuf {
    if cfg!(windows) {
        local_app_data.map_or_else(
            || user_home.join("AppData").join("Local").join("hermes"),
            |dir| dir.join("hermes"),
        )
    } else {
        user_home.join(".hermes")
    }
}
