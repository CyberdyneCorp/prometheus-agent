//! What every `prometheus` process does before running its command: read
//! the launch environment once, resolve the home, install the structured
//! logger, raise the open-file limit and set up crash reports.
//!
//! None of these steps may stop a command that does not need the home
//! (`version`, `help`): a home that cannot be resolved or an invalid
//! `daemon.toml` is reported and the defaults are used.

use std::path::{Path, PathBuf};

use pmt_config::daemon::{DaemonConfig, ProcessConfig};
use pmt_core::env::{EnvPolicy, LaunchEnv};
use pmt_core::error::Error;
use pmt_core::nofile::{self, NofileLimit};
use pmt_core::panic::CrashReporter;
use pmt_core::secret::Redactor;

/// Reads the process environment, once.
///
/// # Errors
///
/// `launch_env.reread` if called twice.
pub fn launch_env() -> Result<LaunchEnv, Error> {
    // Built-in providers' native credential names (`OPENAI_API_KEY`...)
    // are secrets besides `PMT_SECRET_*`; plugin providers name theirs
    // through `PMT_SECRET_<NAME>` or the secrets store.
    let names = pmt_providers::builtin::secret_names();
    LaunchEnv::from_process(&EnvPolicy {
        provider_secret_names: &names,
        replacements: ENV_REPLACEMENTS,
    })
}

/// Non-secret variables people export by habit and the config key that
/// replaces each: they are ignored with a WARN, never read.
pub const ENV_REPLACEMENTS: &[(&str, &str)] = &[
    ("PMT_INFERENCE_PROVIDER", "model.provider"),
    ("PMT_MODEL", "model.default"),
];

/// Logs one WARN per ignored environment variable.
pub fn warn_ignored_env(launch: &LaunchEnv) {
    for var in launch.ignored() {
        tracing::warn!("{}", var.warning());
    }
}

/// Raises the open-file soft limit to `process.nofile_soft_limit` from the
/// home's `daemon.toml` (the default when there is no home or file), and
/// logs a WARN when the result is below that target.
pub fn raise_nofile(home: Option<&Path>) -> Option<NofileLimit> {
    let target = match home.map(DaemonConfig::load) {
        Some(Ok(config)) => config.process.nofile_soft_limit,
        Some(Err(err)) => {
            tracing::warn!("{err}; using the default process.nofile_soft_limit");
            ProcessConfig::default().nofile_soft_limit
        }
        None => ProcessConfig::default().nofile_soft_limit,
    };
    match nofile::raise(target) {
        Ok(Some(limit)) => {
            if limit.below_target() {
                tracing::warn!("{}", limit.warning());
            }
            Some(limit)
        }
        Ok(None) => None,
        Err(err) => {
            tracing::warn!("cannot read the open-file limit: {err}");
            None
        }
    }
}

/// Installs the structured logger: JSON records under `<home>/logs/` and
/// the selected profile's `logs/`, filtered by `PMT_LOG`/`RUST_LOG` (else
/// `logging.level`, applied by [`bind_profile`]), redacting the launch
/// environment's secrets; WARN and above also on an interactive stderr.
pub fn install_logging(home: Option<&PathBuf>, launch: &LaunchEnv) {
    pmt_obs::logs::install(pmt_obs::logs::LogSetup {
        home: home.cloned(),
        env_filter: launch.log_filter().map(str::to_owned),
        redactor: launch_redactor(launch),
        stderr: true,
    });
}

/// Logged at WARN on every start while `security.redact_secrets = false`.
pub const REDACTION_DISABLED_WARNING: &str = "security.redact_secrets is false: stored secret values are not redacted from logs, trajectories, tool results and session storage";

/// Binds the command's logging to the profile it works on: records carry
/// `profile` (so they go to that profile's `logs/`), its `logging.level`
/// applies unless the environment set a filter, and its stored secrets are
/// redacted (unless `security.redact_secrets = false`). A disabled
/// `security.redact_secrets` or `security.protected_instruction_files` and
/// a refused telemetry endpoint are logged at WARN here, on every start.
pub fn bind_profile(scope: &pmt_config::ProfileScope) {
    let name = scope.name().to_string();
    tracing::Span::current().record("profile", name.as_str());
    pmt_tools::protected::warn_if_disabled(&scope.config().security);
    let redact = scope.config().security.redact_secrets;
    if !redact {
        tracing::warn!("{REDACTION_DISABLED_WARNING}");
    }
    if let Some(handle) = pmt_obs::logs::handle() {
        handle.set_level(scope.config().logging.level);
        match scope.secrets().redactor() {
            Ok(redactor) if redact => handle.register_profile(&name, redactor),
            Ok(_) => {}
            Err(err) => tracing::warn!("log redaction without the secret store: {err}"),
        }
    }
    // Logs a refused telemetry endpoint on every start; telemetry is off
    // (no client exists) unless enabled with an acceptable endpoint.
    let _ = pmt_obs::telemetry::Telemetry::resolve(&scope.config().telemetry);
}

/// The launch environment's secrets, named (`PMT_SECRET_X` is `X`).
fn launch_redactor(launch: &LaunchEnv) -> Redactor {
    Redactor::named(launch.secrets().map(|(variable, value)| {
        let name = variable
            .strip_prefix(pmt_core::env::SECRET_PREFIX)
            .unwrap_or(variable);
        (name, value)
    }))
}

/// Crash reports go to `<home>/logs/crash/`, redacting every secret from
/// the launch environment; without a home, to the system temp directory.
#[must_use]
pub fn crash_reporter(home: Option<&PathBuf>, launch: &LaunchEnv) -> CrashReporter {
    let redactor = Redactor::new(launch.secrets().map(|(_, value)| value));
    match home {
        Some(home) => CrashReporter::for_home(home, redactor),
        None => CrashReporter::new(std::env::temp_dir().join("prometheus-crash"), redactor),
    }
}
