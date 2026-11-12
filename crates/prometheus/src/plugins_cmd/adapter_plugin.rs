//! `prometheus plugins test-adapter <plugin dir>` (spec: platform-adapters,
//! Adapter conformance kit): the conformance suite against a
//! `platform-adapter` plugin. The plugin is installed in a throwaway home,
//! granted exactly what it declares plus the kit's [`Reference`] platform
//! (passed as its `api` option), built by the gateway's plugin adapter
//! factory and run through the same suite as a first-party adapter. Its
//! capability-grant check fails when any host call was refused during the
//! run.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::error::{Error, codes};
use pmt_gateway::messaging::AdapterSetup;
use pmt_gateway::messaging::conformance::{self, Reference, Report};
use pmt_plugins::record::{Flags, InstallRecord, Origin};

use super::{inspect, manage};
use crate::plugins_host;

/// The suite's check of the plugin's grants.
const GRANTS_CHECK: &str = "capability grants";

/// Runs the suite against the plugin at `dir`; exit 0 only when no check
/// failed.
pub(super) fn test_plugin(dir: &Path) -> Result<u8, Error> {
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
    let setup = install(home.path(), dir, &manifest.id, &platform, reference.base())?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            Error::new(
                &codes::TASK_PANICKED,
                format!("cannot start the runtime: {e}"),
            )
        })?;
    let report = runtime.block_on(run(&setup, &manifest.id, reference))?;
    crate::output::print_raw(&report.render())?;
    Ok(u8::from(!report.passed()))
}

/// Builds the adapter through the gateway's factory and runs the suite,
/// settling the capability-grant check from what the plugin was refused.
async fn run(setup: &AdapterSetup, id: &str, reference: Reference) -> Result<Report, Error> {
    let built = (plugins_host::adapter::factory())(setup);
    let loaded = plugins_host::current(&setup.scope, Duration::from_secs(5)).current();
    let adapter = built.map_err(|err| {
        // Why the plugin did not load says more than "no adapter".
        let problem = loaded
            .statuses
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.problem.clone());
        problem.map_or(err, |problem| Error::new(&codes::PLUGINS_LOAD, problem))
    })?;
    let descriptor = adapter.describe();
    reference.enforce(descriptor.max_message_length, descriptor.length_unit);
    let mut report = conformance::run(adapter, Arc::new(reference)).await;
    let refused = loaded
        .plugins
        .iter()
        .find(|p| p.manifest.id == id)
        .map(|p| p.instance.context().grants.refused())
        .unwrap_or_default();
    let outcome = if refused.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the plugin was refused capabilities it was not granted: {}",
            refused.join(", ")
        ))
    };
    report.set(GRANTS_CHECK, outcome);
    Ok(report)
}

/// Installs a copy of the plugin in profile `default` of `home`, enabled
/// for `platform` with `api` as its platform, adopted and granted what it
/// declares plus the mock platform's address.
fn install(
    home: &Path,
    dir: &Path,
    id: &str,
    platform: &str,
    api: &str,
) -> Result<AdapterSetup, Error> {
    let layout = Home::open(home)?;
    let profile = layout.profile(&ProfileName::parse("default")?)?;
    let installed = profile.path().join("plugins").join(id);
    pmt_core::private_fs::copy_tree(dir, &installed)
        .map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, &installed, &e))?;
    grant_reference(&installed, api)?;
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
    let any = serde_json::json!({"type": "object", "additionalProperties": true});
    pmt_config::platforms::register_plugin_platform(platform, Some(any));
    let config = format!(
        "[plugins]\nenabled = [\"{id}\"]\n\n[gateway.platforms.{platform}]\nenabled = true\n\
         options = {{ api = \"{api}\" }}\n"
    );
    let file = profile.path().join("config.toml");
    std::fs::write(&file, config).map_err(|e| Error::io(&codes::HOME_IO, &file, &e))?;
    let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other)?;
    let target = manage::target(&scope, id)?;
    manage::approve(
        &scope,
        &target.manifest,
        &target.identity,
        &target.requested,
    )?;
    let config = scope.config().gateway.platform(platform);
    Ok(AdapterSetup {
        scope,
        platform: platform.to_owned(),
        config,
    })
}

/// Adds the mock platform's address to the copy's declared network.
fn grant_reference(installed: &Path, api: &str) -> Result<(), Error> {
    let file = installed.join("plugin.toml");
    let text =
        std::fs::read_to_string(&file).map_err(|e| Error::io(&codes::PLUGINS_LOAD, &file, &e))?;
    let mut manifest: toml::Table = text
        .parse()
        .map_err(|e| Error::new(&codes::PLUGINS_MANIFEST, format!("{}: {e}", file.display())))?;
    let address = api.trim_start_matches("http://").to_owned();
    let capabilities = manifest
        .entry("capabilities")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let Some(table) = capabilities.as_table_mut() {
        let network = table
            .entry("network")
            .or_insert_with(|| toml::Value::Array(Vec::new()));
        if let Some(list) = network.as_array_mut() {
            list.push(toml::Value::String(address));
        }
    }
    std::fs::write(&file, manifest.to_string())
        .map_err(|e| Error::io(&codes::PLUGINS_LOAD, &file, &e))
}
