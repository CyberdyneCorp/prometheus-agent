//! Which `cua-driver` runs and whether it fits (spec: computer-use, Driver
//! runtime management and identity; Backend selection and availability).
//!
//! The driver is `computer_use.driver_path` when set (the user's binary:
//! never repaired or replaced) or the managed `cua-driver` runtime of the
//! home's store, installed lazily under the `[runtimes]` policy. Before a
//! session drives with it, its `manifest` is checked against the contract
//! this build speaks: version 0.20 or newer, an MCP launch command and the
//! subcommand flags the private runtimes use.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use pmt_config::daemon::DaemonConfig;
use pmt_config::schema::ComputerUseConfig;
use pmt_update::runtimes::Store;
use serde_json::Value;

/// The managed runtime's name.
pub const RUNTIME: &str = "cua-driver";
/// The oldest driver version this build drives.
pub const MIN_VERSION: (u64, u64, u64) = (0, 20, 0);
/// The subcommand flags the contract needs.
const CONTRACT: &[(&str, &[&str])] = &[
    ("mcp", &["--socket"]),
    (
        "serve",
        &[
            "--socket",
            "--permission-mode",
            "--capability-manifest",
            "--approve-capability-manifest",
            "--embedded",
        ],
    ),
    ("stop", &["--socket"]),
];
/// How long a probe verb may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// The install hint of a missing driver.
pub const INSTALL_HINT: &str =
    "run `prometheus computer-use install` (or set computer_use.driver_path)";

/// Where a driver binary comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `computer_use.driver_path`.
    Configured,
    /// The managed runtime store.
    Managed,
}

/// A driver binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Driver {
    /// Its path.
    pub path: PathBuf,
    /// Where it comes from.
    pub source: Source,
}

/// The installed driver, if any; `Err` says why none can be used without
/// an explicit install (the `unavailable:<reason>` of the tool).
///
/// # Errors
///
/// The reason (a missing configured binary, no build for this host, an
/// exclusion, lazy installs disabled).
pub fn installed(
    home: &Path,
    config: &ComputerUseConfig,
    daemon: &DaemonConfig,
) -> Result<Option<Driver>, String> {
    if let Some(path) = &config.driver_path {
        return if path.is_file() {
            Ok(Some(Driver {
                path: path.clone(),
                source: Source::Configured,
            }))
        } else {
            Err(format!(
                "computer_use.driver_path {} does not exist",
                path.display()
            ))
        };
    }
    let store = Store::open(home);
    if let Err(err) = store.pinned(RUNTIME) {
        return Err(err.message().to_owned());
    }
    if let Some(installed) = store.installed(RUNTIME) {
        return Ok(Some(Driver {
            path: installed.exe(),
            source: Source::Managed,
        }));
    }
    if daemon.runtimes.exclude.iter().any(|name| name == RUNTIME) {
        return Err(format!("{RUNTIME} is excluded (runtimes.exclude)"));
    }
    if !daemon.runtimes.allow_lazy_install {
        return Err(format!(
            "{RUNTIME} is not installed and lazy installs are disabled"
        ));
    }
    Ok(None)
}

/// What a driver's `manifest` verb reports.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest {
    /// `binary_version`.
    pub version: String,
    /// The MCP launch command (`None`: the binary itself).
    pub mcp_command: Option<PathBuf>,
    /// The MCP launch arguments.
    pub mcp_args: Vec<String>,
    /// Advertised flags by subcommand.
    pub subcommands: BTreeMap<String, Vec<String>>,
    /// The driver mentions `--no-overlay`.
    pub no_overlay: bool,
}

impl Manifest {
    /// Parses the manifest JSON.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text.trim()).ok()?;
        let invocation = &value["mcp_invocation"];
        let subcommands = value["subcommands"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|command| {
                let name = command["name"].as_str()?.to_owned();
                let args = command["args"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|arg| arg["name"].as_str().map(str::to_owned))
                    .collect();
                Some((name, args))
            })
            .collect::<BTreeMap<String, Vec<String>>>();
        let no_overlay = subcommands
            .values()
            .flatten()
            .any(|flag| flag == "--no-overlay");
        Some(Self {
            version: value["binary_version"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_owned(),
            mcp_command: invocation["command"]
                .as_str()
                .filter(|c| c.contains(['/', '\\']))
                .map(PathBuf::from),
            mcp_args: invocation["args"]
                .as_array()
                .map(|args| {
                    args.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            subcommands,
            no_overlay,
        })
    }

    /// Why the manifest falls outside the contract, if it does.
    #[must_use]
    pub fn problem(&self) -> Option<String> {
        let Some(version) = semver(&self.version) else {
            return Some(format!(
                "the driver reports no semantic version ('{}')",
                self.version
            ));
        };
        if version < MIN_VERSION {
            let (a, b, c) = MIN_VERSION;
            return Some(format!(
                "cua-driver {} is older than the supported {a}.{b}.{c}",
                self.version
            ));
        }
        if self.mcp_args.is_empty() {
            return Some("the driver manifest has no MCP launch command".to_owned());
        }
        let missing: Vec<String> = CONTRACT
            .iter()
            .flat_map(|(command, flags)| {
                let have = self.subcommands.get(*command);
                flags
                    .iter()
                    .filter(move |flag| !have.is_some_and(|h| h.iter().any(|f| f == *flag)))
                    .map(move |flag| format!("{command} {flag}"))
            })
            .collect();
        (!missing.is_empty()).then(|| format!("the driver lacks {}", missing.join(", ")))
    }
}

/// `v0.21.3` / `0.21.3-beta` as numbers.
fn semver(text: &str) -> Option<(u64, u64, u64)> {
    let core = text.trim_start_matches('v').split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// Runs a short driver verb with `env` (stdin closed; older drivers read
/// stdin on unknown verbs) and returns its stdout.
///
/// # Errors
///
/// The spawn failure, timeout or failing exit with the driver's stderr.
pub fn verb(driver: &Path, args: &[&str], env: &[(String, OsString)]) -> Result<String, String> {
    let mut command = Command::new(driver);
    command
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = pmt_sandbox::container::run_bounded(command, PROBE_TIMEOUT)
        .map_err(|e| format!("{} {}: {}", driver.display(), args.join(" "), e))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "{} {} failed: {}",
            driver.display(),
            args.join(" "),
            stderr.lines().last().unwrap_or("no output")
        ))
    }
}

/// The driver's manifest, checked against the contract.
///
/// # Errors
///
/// Why the driver cannot be used.
pub fn checked(driver: &Path, env: &[(String, OsString)]) -> Result<Manifest, String> {
    let text = verb(driver, &["manifest"], env)?;
    let manifest = Manifest::parse(&text).ok_or("the driver manifest is not valid JSON")?;
    match manifest.problem() {
        Some(problem) => Err(problem),
        None => Ok(manifest),
    }
}

#[cfg(test)]
mod tests {
    use super::Manifest;

    const GOOD: &str = r#"{"binary_version": "0.21.0",
        "mcp_invocation": {"command": "cua-driver", "args": ["mcp"]},
        "subcommands": [
            {"name": "mcp", "args": [{"name": "--socket"}, {"name": "--no-overlay"}]},
            {"name": "serve", "args": [{"name": "--socket"}, {"name": "--permission-mode"}, {"name": "--capability-manifest"}, {"name": "--approve-capability-manifest"}, {"name": "--embedded"}]},
            {"name": "stop", "args": [{"name": "--socket"}]}
        ]}"#;

    #[test]
    fn the_contract_needs_a_recent_version_and_the_runtime_flags() {
        let good = Manifest::parse(GOOD).unwrap();
        assert_eq!(good.problem(), None);
        assert!(good.no_overlay);
        assert_eq!(
            good.mcp_command, None,
            "a bare name keeps the resolved binary"
        );
        let old = Manifest::parse(&GOOD.replace("0.21.0", "0.19.4")).unwrap();
        assert!(
            old.problem()
                .unwrap()
                .contains("older than the supported 0.20.0")
        );
        let partial = Manifest::parse(&GOOD.replace(
            r#"{"name": "stop", "args": [{"name": "--socket"}]}"#,
            r#"{"name": "stop", "args": []}"#,
        ))
        .unwrap();
        assert_eq!(partial.problem().unwrap(), "the driver lacks stop --socket");
    }
}
