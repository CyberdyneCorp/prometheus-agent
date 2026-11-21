//! The binary's side of a Hermes migration: the registries that live
//! here (toolsets, themes, adapters, providers, installed plugins), the
//! scheduled-job catalog and the Python-bridge plugin import.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use pmt_config::layout::ProfileName;
use pmt_config::{Launch, Overrides, ProfileScope, platforms};
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_migrate::{Host, PluginOutcome, Registry};
use pmt_plugins::catalog::Catalog;
use pmt_plugins::python::audit::Verdict;
use pmt_providers::registry::ProviderRegistry;

use super::interrupt::Interrupt;
use crate::cron_host::HostCatalog;
use crate::plugins_host::bridge::migrate;

/// The host of `prometheus migrate from-hermes`.
pub(super) struct MigrateHost<'a> {
    launch: &'a LaunchEnv,
    providers: Option<ProviderRegistry>,
    interrupt: Interrupt,
    /// The profile whose plugin catalog answers when no target profile is
    /// named: `--into-profile`, else the home's default.
    catalog_profile: Option<(PathBuf, String)>,
    catalog: OnceLock<Catalog>,
}

impl<'a> MigrateHost<'a> {
    /// The host for this process.
    #[must_use]
    pub(super) fn new(launch: &'a LaunchEnv, interrupt: Interrupt) -> Self {
        let providers =
            ProviderRegistry::for_profile(&pmt_config::Config::default(), Vec::new()).ok();
        Self {
            launch,
            providers,
            interrupt,
            catalog_profile: None,
            catalog: OnceLock::new(),
        }
    }

    /// Resolves catalog plugins with the catalog of `profile` of `home`.
    #[must_use]
    pub(super) fn with_catalog_of(mut self, home: PathBuf, profile: Option<String>) -> Self {
        let profile = profile.unwrap_or_else(|| ProfileName::default_profile().as_str().to_owned());
        self.catalog_profile = Some((home, profile));
        self
    }

    /// The plugin catalog: `scope`'s when given, else the configured
    /// profile's (the embedded catalog when it cannot be opened).
    fn catalog(&self, scope: Option<&ProfileScope>) -> Catalog {
        if let Some(scope) = scope {
            return crate::plugins_host::catalog::current(scope, true);
        }
        self.catalog
            .get_or_init(|| {
                self.catalog_profile
                    .as_ref()
                    .and_then(|(home, profile)| open(home, profile))
                    .map_or_else(
                        || Catalog::merged(None),
                        |scope| crate::plugins_host::catalog::current(&scope, true),
                    )
            })
            .clone()
    }
}

/// `profile` of `home`, when it exists and loads.
fn open(home: &Path, profile: &str) -> Option<ProfileScope> {
    let name = ProfileName::parse(profile).ok()?;
    let dir = pmt_config::layout::Home::open(home)
        .ok()?
        .existing_profile(&name)
        .ok()?;
    ProfileScope::open(dir, &Overrides::default(), Launch::Other).ok()
}

impl Host for MigrateHost<'_> {
    fn known(&self, registry: Registry, name: &str, scope: Option<&ProfileScope>) -> bool {
        match registry {
            Registry::Toolset => crate::tools_cmd::expand_toolsets(&[name.to_owned()]).is_ok(),
            Registry::Provider => self
                .providers
                .as_ref()
                .is_some_and(|r| r.canonical(name).is_some()),
            Registry::Theme => crate::chat::theme::BUILTIN.contains(&name),
            Registry::Platform => platforms::first_party(name).is_some(),
            Registry::MemoryProvider => scope.is_some_and(|s| {
                let dir = s.dir().join("plugins").join(name);
                dir.join("plugin.toml").is_file() || dir.join("plugin.yaml").is_file()
            }),
            Registry::CatalogPlugin => self.catalog(scope).find(name).is_some(),
        }
    }

    fn known_secret(&self, name: &str) -> bool {
        let provider_key = self
            .providers
            .as_ref()
            .is_some_and(|r| r.iter().any(|p| p.secret_names().iter().any(|s| s == name)));
        let adapter_token = platforms::FIRST_PARTY.iter().any(|p| {
            name.starts_with(&format!("{}_", p.name.to_ascii_uppercase()))
                && name.ends_with("TOKEN")
        });
        provider_key || adapter_token
    }

    fn cron_catalog(&self, scope: &ProfileScope) -> Box<dyn pmt_cron::validate::Catalog> {
        Box::new(HostCatalog::new(scope, self.launch))
    }

    fn import_plugins(
        &self,
        scope: &ProfileScope,
        dir: &Path,
        enabled: &[String],
        skip: &[String],
    ) -> Result<Vec<PluginOutcome>, Error> {
        let report = migrate::import(scope, dir, enabled, skip)?;
        Ok(report
            .plugins
            .into_iter()
            .map(|p| PluginOutcome {
                bridge_compatible: p.verdict != Verdict::Incompatible,
                id: p.id,
                was_enabled: p.was_enabled,
                blocking: p.blocking,
                proposed: p.proposed,
                skipped: p.skipped,
            })
            .collect())
    }

    fn install_catalog_plugin(&self, scope: &ProfileScope, id: &str) -> Result<(), Error> {
        crate::plugins_cmd::install::install_from_catalog(scope, id)?;
        // The new adapter's platform names a valid table from now on.
        crate::plugins_host::adapter::register_installed(scope.dir(), id);
        Ok(())
    }

    fn progress(&self, line: &str) {
        eprintln!("{line}");
    }

    fn interrupted(&self) -> bool {
        self.interrupt.raised()
    }
}
