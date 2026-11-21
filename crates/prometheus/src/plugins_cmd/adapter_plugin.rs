//! `prometheus plugins test-adapter <plugin dir>` (spec: platform-adapters,
//! Adapter conformance kit): the conformance suite against a
//! `platform-adapter` plugin. The plugin is installed in a throwaway home,
//! granted exactly what it declares plus the kit's [`Reference`] platform
//! (passed as its `api` option), built by the gateway's plugin adapter
//! factory and run through the same suite as a first-party adapter. Its
//! capability-grant check fails when any host call was refused during the
//! run.
//!
//! [`PluginKit`] is the same setup for a catalog adapter's own tests: the
//! plugin under the options and hosts of a mock of its real platform.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::error::{Error, codes};
use pmt_gateway::messaging::conformance::{self, MockPlatform, Reference, Report};
use pmt_gateway::messaging::{Adapter, AdapterSetup};
use pmt_plugins::record::{Flags, InstallRecord, Origin};
use serde_json::{Value, json};

use super::{inspect, manage};
use crate::plugins_host;

/// The suite's check of the plugin's grants.
const GRANTS_CHECK: &str = "capability grants";

/// Runs the suite against the plugin at `dir`; exit 0 only when no check
/// failed.
pub(super) fn test_plugin(dir: &Path) -> Result<u8, Error> {
    let home = tempfile::Builder::new()
        .prefix("prometheus-test-adapter-")
        .tempdir()
        .map_err(|e| Error::io(&codes::HOME_IO, &std::env::temp_dir(), &e))?;
    let reference = Reference::start().map_err(|e| {
        Error::new(
            &codes::HOME_IO,
            format!("cannot start the mock platform: {e}"),
        )
    })?;
    let api = reference.base().to_owned();
    let host = api.trim_start_matches("http://").to_owned();
    let kit = PluginKit::install(home.path(), dir, &json!({ "api": api }), &[host])?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            Error::new(
                &codes::TASK_PANICKED,
                format!("cannot start the runtime: {e}"),
            )
        })?;
    let report = runtime.block_on(async {
        let adapter = kit.adapter()?;
        let descriptor = adapter.describe();
        reference.enforce(descriptor.max_message_length, descriptor.length_unit);
        Ok::<_, Error>(kit.run(adapter, Arc::new(reference)).await)
    })?;
    crate::output::print_raw(&report.render())?;
    Ok(u8::from(!report.passed()))
}

/// An adapter plugin installed in profile `default` of a home, enabled
/// for its platform with given options, adopted and granted what it
/// declares plus the mock platform's hosts.
pub struct PluginKit {
    setup: AdapterSetup,
    id: String,
    dir: PathBuf,
}

impl PluginKit {
    /// Installs a copy of the plugin at `dir` in `home` with `options`
    /// (the platform table's `options`) and `hosts` added to its declared
    /// network.
    ///
    /// # Errors
    ///
    /// The plugin is not a `platform-adapter` or cannot be installed.
    pub fn install(
        home: &Path,
        dir: &Path,
        options: &Value,
        hosts: &[String],
    ) -> Result<Self, Error> {
        let manifest = inspect::check_dir(dir)?;
        let Some(platform) = manifest.adapter_platform() else {
            return Err(Error::new(
                &codes::CLI_USAGE,
                format!(
                    "plugin '{}' does not implement the platform-adapter world",
                    manifest.id
                ),
            ));
        };
        let (setup, installed) = install(home, dir, &manifest.id, &platform, options, hosts)?;
        Ok(Self {
            setup,
            id: manifest.id,
            dir: installed,
        })
    }

    /// The installed plugin's directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The profile the plugin is installed in.
    #[must_use]
    pub fn setup(&self) -> &AdapterSetup {
        &self.setup
    }

    /// Builds the adapter through the gateway's plugin adapter factory.
    ///
    /// # Errors
    ///
    /// The plugin does not load (the error says why).
    pub fn adapter(&self) -> Result<Arc<dyn Adapter>, Error> {
        let built = (plugins_host::adapter::factory())(&self.setup);
        built.map_err(|err| {
            // Why the plugin did not load says more than "no adapter".
            let loaded = plugins_host::current(&self.setup.scope, Duration::from_secs(5)).current();
            let problem = loaded
                .statuses
                .iter()
                .find(|s| s.id == self.id)
                .and_then(|s| s.problem.clone());
            problem.map_or(err, |problem| Error::new(&codes::PLUGINS_LOAD, problem))
        })
    }

    /// The host calls refused to the plugin so far (outside its grants).
    #[must_use]
    pub fn refused(&self) -> Vec<String> {
        plugins_host::current(&self.setup.scope, Duration::from_secs(5))
            .current()
            .plugins
            .iter()
            .find(|p| p.manifest.id == self.id)
            .map(|p| p.instance.context().grants.refused())
            .unwrap_or_default()
    }

    /// Runs the suite against `adapter` (built by [`Self::adapter`]) and
    /// `platform`, settling the capability-grant check from what the
    /// plugin was refused.
    pub async fn run(&self, adapter: Arc<dyn Adapter>, platform: Arc<dyn MockPlatform>) -> Report {
        let mut report = conformance::run(adapter, platform).await;
        let refused = self.refused();
        let outcome = if refused.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "the plugin was refused capabilities it was not granted: {}",
                refused.join(", ")
            ))
        };
        report.set(GRANTS_CHECK, outcome);
        report
    }
}

/// Installs a copy of the plugin in profile `default` of `home`, enabled
/// for `platform` with `options`, adopted and granted what it declares
/// plus `hosts`.
fn install(
    home: &Path,
    dir: &Path,
    id: &str,
    platform: &str,
    options: &Value,
    hosts: &[String],
) -> Result<(AdapterSetup, PathBuf), Error> {
    let layout = Home::open(home)?;
    let profile = layout.profile(&ProfileName::parse("default")?)?;
    let installed = profile.path().join("plugins").join(id);
    pmt_core::private_fs::copy_tree(dir, &installed)
        .map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, &installed, &e))?;
    grant_hosts(&installed, hosts)?;
    InstallRecord {
        source: Origin::Adopted,
        location: installed.to_string_lossy().into_owned(),
        r#ref: None,
        package_digest: None,
        digest: pmt_plugins::digest::of_dir(&installed)?,
        signer: None,
        tier: None,
        installed_at: pmt_plugins::install::now(),
        flags: Flags::default(),
        held_update: None,
    }
    .write(&installed)?;
    // The platform validates once it is declared (the plugin publishes no
    // options schema: its `start` checks the options).
    let any = json!({"type": "object", "additionalProperties": true});
    pmt_config::platforms::register_plugin_platform(platform, Some(any));
    let mut config = toml::Table::new();
    let mut plugins = toml::Table::new();
    plugins.insert("enabled".into(), toml::Value::Array(vec![id.into()]));
    config.insert("plugins".into(), toml::Value::Table(plugins));
    let table = toml::Value::try_from(json!({
        "gateway": {"platforms": {platform: {"enabled": true, "options": options}}}
    }))
    .map_err(|e| Error::new(&codes::CONFIG_INVALID, format!("adapter options: {e}")))?;
    if let toml::Value::Table(table) = table {
        config.extend(table);
    }
    let file = profile.path().join("config.toml");
    std::fs::write(&file, config.to_string()).map_err(|e| Error::io(&codes::HOME_IO, &file, &e))?;
    let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other)?;
    let target = manage::target(&scope, id)?;
    manage::approve(
        &scope,
        &target.manifest,
        &target.identity,
        &target.requested,
    )?;
    let config = scope.config().gateway.platform(platform);
    Ok((
        AdapterSetup {
            scope,
            platform: platform.to_owned(),
            config,
        },
        installed,
    ))
}

/// Adds `hosts` to the copy's declared network.
fn grant_hosts(installed: &Path, hosts: &[String]) -> Result<(), Error> {
    let file = installed.join("plugin.toml");
    let text =
        std::fs::read_to_string(&file).map_err(|e| Error::io(&codes::PLUGINS_LOAD, &file, &e))?;
    let mut manifest: toml::Table = text
        .parse()
        .map_err(|e| Error::new(&codes::PLUGINS_MANIFEST, format!("{}: {e}", file.display())))?;
    let capabilities = manifest
        .entry("capabilities")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let Some(table) = capabilities.as_table_mut() {
        let network = table
            .entry("network")
            .or_insert_with(|| toml::Value::Array(Vec::new()));
        if let Some(list) = network.as_array_mut() {
            list.extend(hosts.iter().cloned().map(toml::Value::String));
        }
    }
    std::fs::write(&file, manifest.to_string())
        .map_err(|e| Error::io(&codes::PLUGINS_LOAD, &file, &e))
}
