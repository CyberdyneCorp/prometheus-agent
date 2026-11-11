//! The binary's side of a Hermes migration: the registries that live
//! here (toolsets, themes, adapters, providers, installed plugins), the
//! scheduled-job catalog and the Python-bridge plugin import.

use std::path::Path;

use pmt_config::{ProfileScope, platforms};
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_migrate::{Host, PluginOutcome, Registry};
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
        }
    }
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
    ) -> Result<Vec<PluginOutcome>, Error> {
        let report = migrate::import(scope, dir, enabled)?;
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

    fn progress(&self, line: &str) {
        eprintln!("{line}");
    }

    fn interrupted(&self) -> bool {
        self.interrupt.raised()
    }
}
