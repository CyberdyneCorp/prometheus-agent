//! Unit-test helpers: a fake [`Host`] and a [`Source`] over a temporary
//! Hermes profile.

use std::path::Path;

use pmt_config::ProfileScope;
use pmt_core::error::Error;

use crate::env::{self, EnvEntry};
use crate::host::{Host, PluginOutcome, Registry};
use crate::plan::Source;
use crate::source::SourceProfile;
use crate::yaml::{self, Doc};

/// Knows a few names of each registry.
pub(crate) struct FakeHost;

struct AnyJob;

impl pmt_cron::validate::Catalog for AnyJob {
    fn skill_exists(&self, _: &str) -> bool {
        true
    }
    fn toolset_exists(&self, _: &str) -> bool {
        true
    }
    fn check_model(&self, _: Option<&str>, _: Option<&str>) -> Result<(), String> {
        Ok(())
    }
    fn profile_exists(&self, _: &str) -> bool {
        true
    }
    fn secret_reserved(&self, _: &str) -> bool {
        false
    }
}

impl Host for FakeHost {
    fn known(&self, registry: Registry, name: &str, _: Option<&ProfileScope>) -> bool {
        let names: &[&str] = match registry {
            Registry::Toolset => &["terminal", "web", "file"],
            Registry::Provider => &["anthropic", "openrouter"],
            Registry::Theme => &["mono"],
            Registry::Platform => &["telegram", "discord"],
            Registry::MemoryProvider => &[],
        };
        names.contains(&name)
    }

    fn known_secret(&self, _: &str) -> bool {
        false
    }

    fn cron_catalog(&self, _: &ProfileScope) -> Box<dyn pmt_cron::validate::Catalog> {
        Box::new(AnyJob)
    }

    fn import_plugins(
        &self,
        _: &ProfileScope,
        _: &Path,
        _: &[String],
    ) -> Result<Vec<PluginOutcome>, Error> {
        Ok(Vec::new())
    }

    fn progress(&self, _: &str) {}
}

/// A Hermes profile in a temporary directory and what a [`Source`]
/// needs of it.
pub(crate) struct Fixture {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) profile: SourceProfile,
    pub(crate) config: Doc,
    pub(crate) env: Vec<EnvEntry>,
}

impl Fixture {
    /// A profile with `config.yaml` text `config` and `.env` text `env`.
    pub(crate) fn new(config: &str, env: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let profile = SourceProfile {
            name: "default".to_owned(),
            dir: dir.path().to_owned(),
        };
        Self {
            profile,
            config: yaml::parse(config).unwrap(),
            env: env::parse(env),
            dir,
        }
    }

    /// The source over the fixture.
    pub(crate) fn source(&self) -> Source<'_> {
        Source {
            profile: &self.profile,
            config: &self.config,
            env: &self.env,
            host: &FakeHost,
            enable_cron: false,
            staging: self.dir.path(),
        }
    }
}
