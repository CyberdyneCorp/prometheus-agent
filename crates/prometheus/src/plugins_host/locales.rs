//! The enabled language packs of a profile (spec: i18n, Language-pack
//! plugins), for `display.language` checks and every surface's strings.

use std::path::Path;

use pmt_config::Config;
use pmt_i18n::PackLocale;
use pmt_plugins::catalog::Cache;
use pmt_plugins::load::{LoadInputs, survey};

/// The languages of the enabled, approved plugins of the profile at
/// `profile_dir` under `config`, in load order. A survey reads manifests
/// only: nothing is instantiated.
#[must_use]
pub fn language_packs(config: &Config, profile_dir: &Path) -> Vec<PackLocale> {
    let profile = profile_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let running = super::running_version();
    let blocked = Cache::of(profile_dir).kill_list(&super::catalog::verify);
    let inputs = LoadInputs {
        profile: &profile,
        profile_dir,
        cwd: None,
        config: &config.plugins,
        embedded: &[],
        running: &running,
        blocked: &blocked,
        profile_config: Some(config),
    };
    pmt_plugins::locales::packs(&survey(&inputs))
}
