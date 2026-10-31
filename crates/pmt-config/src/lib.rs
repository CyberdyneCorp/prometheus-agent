//! Configuration, home layout and profile scope.
//!
//! - [`home`] resolves the home directory and [`layout`] creates the D13
//!   layout ([`Home`], [`ProfileName`], [`ProfileDir`]).
//! - [`schema`] is the typed `config.toml` ([`Config`]), the single source
//!   of defaults; [`catalog`] reads it as dotted keys, [`validate`] checks
//!   documents against it and [`reference`] renders its documentation.
//! - [`ProfileScope`] is the only way to an effective configuration:
//!   [`layers`] stacks defaults, the file and `--set` overrides.
//! - [`write::ConfigFile`] is the one format-preserving writer and
//!   [`migrate`] brings older files forward.
//! - [`secrets`] is the per-profile secrets store and resolver.
//! - [`daemon`] is the machine-wide `daemon.toml`.

mod backups;
pub mod catalog;
pub mod credential;
pub mod daemon;
mod edit;
pub mod essential;
pub mod grants;
pub mod home;
pub mod layers;
pub mod layout;
pub mod migrate;
mod parse;
pub mod passthrough;
pub mod profile_id;
pub mod profiles;
pub mod provider_urls;
pub mod reference;
pub mod schema;
pub mod scope;
pub mod secrets;
pub mod validate;
pub mod write;

pub use daemon::DaemonConfig;
pub use layers::{Layered, Overrides, Source};
pub use layout::{Home, ProfileDir, ProfileName};
pub use schema::{Config, SCHEMA_VERSION};
pub use scope::{Launch, ProfileScope};
pub use secrets::{SecretStore, SecretsHandle};
pub use write::ConfigFile;

/// Validates `text` as a `config.toml` named `path` (current schema
/// version, schema, no credentials): what `config validate` and the editor
/// loop check.
///
/// # Errors
///
/// Every problem in one error (exit 78), a credential in a non-secret key
/// included.
pub fn check(path: &std::path::Path, text: &str) -> Result<(), pmt_core::Error> {
    write::check_document(path, text)
}

/// Checks that the `config.toml` at `path` would load in this build
/// (migrated in memory when older), writing nothing: what the self-test of
/// a staged update runs against every profile. A missing file loads as the
/// defaults.
///
/// # Errors
///
/// `config.too_new` for a file from a newer binary, otherwise what
/// [`check`] reports; `home.io` when the file cannot be read.
pub fn check_readonly(path: &std::path::Path) -> Result<(), pmt_core::Error> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(pmt_core::Error::new(
                &pmt_core::error::codes::HOME_IO,
                format!("cannot read {}: {e}", path.display()),
            ));
        }
    };
    let (_, migrated) = migrate::in_memory(path, &text)?;
    write::check_document(path, &migrated)
}
