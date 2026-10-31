//! The file, config, secrets, store, daemon, runtime, plugin,
//! observability and provider groups.

use std::path::{Path, PathBuf};
use std::time::Duration;

use pmt_core::doctor::{Check, Status};
use pmt_core::error::Error;
use pmt_core::runtime::{Role, live_record};

use super::Inputs;
use crate::setup_cmd::sections::required_secrets;

/// How long an `--online` provider probe may take.
const PROBE: Duration = Duration::from_secs(2);

/// The profiles under `<home>/profiles/`, sorted.
#[must_use]
pub(crate) fn profile_names(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root.join("profiles")) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// The owner-only paths of a home and profile: directories (0700) and
/// files (0600). Missing files are skipped.
pub(super) fn private_paths(root: &Path, profile: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let dirs = vec![
        root.to_path_buf(),
        root.join("profiles"),
        root.join("run"),
        root.join("logs"),
        profile.to_path_buf(),
    ];
    let files = ["config.toml", "secrets.toml", "state.db", "grants.toml"]
        .iter()
        .map(|f| profile.join(f))
        .collect();
    (dirs, files)
}

/// The group-and-other mode bits of `path` when any is set (POSIX).
#[must_use]
pub(crate) fn loose_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(path).ok()?.permissions().mode() & 0o777;
        (mode & 0o077 != 0).then_some(mode)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn mode_check(group: &'static str, name: &'static str, paths: &[PathBuf]) -> Check {
    let loose: Vec<String> = paths
        .iter()
        .filter(|p| p.exists())
        .filter_map(|p| loose_mode(p).map(|m| format!("{} ({m:o})", p.display())))
        .collect();
    if loose.is_empty() {
        Check::new(
            group,
            name,
            Status::Ok,
            format!("{name}: owner-only permissions"),
        )
    } else {
        Check::new(
            group,
            name,
            Status::Fail,
            format!("readable by others: {}", loose.join(", ")),
        )
        .with_hint("prometheus doctor --fix")
    }
}

/// The profile a run checks, read without opening the home (opening it
/// would tighten what the checks must report): `-p`, else
/// `<home>/active_profile`, else `default`.
pub(super) fn raw_profile(root: &Path, flag: Option<&str>) -> String {
    flag.map(str::to_owned)
        .or_else(|| {
            std::fs::read_to_string(root.join("active_profile"))
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "default".to_owned())
}

/// `home` (directories 0700, profile files 0600) and the `secrets` store
/// mode, read before the home is opened.
pub(super) fn permissions(root: &Path, profile: &Path) -> (Vec<Check>, Check) {
    let (dirs, files) = private_paths(root, profile);
    let (secrets, files): (Vec<PathBuf>, Vec<PathBuf>) =
        files.into_iter().partition(|f| f.ends_with("secrets.toml"));
    (
        vec![
            mode_check("home", "directories", &dirs),
            mode_check("home", "files", &files),
        ],
        mode_check("secrets", "permissions", &secrets),
    )
}

/// `config`: validity (the scope opened), pending migrations, ignored
/// environment variables.
pub(super) fn config(inputs: &Inputs<'_>) -> Vec<Check> {
    let file = inputs.scope.profile().config_file();
    let mut checks = vec![Check::new(
        "config",
        "valid",
        Status::Ok,
        format!("{} is valid", file.path().display()),
    )];
    checks.push(match fix::pending_migration(&file) {
        Some(from) => Check::new(
            "config",
            "schema_version",
            Status::Warn,
            format!("config.toml is at schema version {from}; a migration is pending"),
        )
        .with_hint("prometheus doctor --fix"),
        None => Check::new(
            "config",
            "schema_version",
            Status::Ok,
            "schema version current",
        ),
    });
    for ignored in inputs.ctx.launch.ignored() {
        checks.push(Check::new(
            "config",
            "ignored_env",
            Status::Warn,
            ignored.doctor_message(),
        ));
    }
    checks
}

use super::fix;

/// `secrets`: store permissions and referenced-but-missing secrets.
pub(super) fn secrets(inputs: &Inputs<'_>) -> Result<Vec<Check>, Error> {
    let mut checks = Vec::new();
    for secret in required_secrets(inputs.scope)? {
        checks.push(if secret.present {
            Check::new(
                "secrets",
                "required",
                Status::Ok,
                format!("{} is set ({})", secret.name, secret.provider),
            )
        } else {
            Check::new(
                "secrets",
                "required",
                Status::Fail,
                format!(
                    "{} is missing (provider {} needs it)",
                    secret.name, secret.provider
                ),
            )
            .with_hint(format!("prometheus secrets set {}", secret.name))
        });
    }
    Ok(checks)
}

/// `store`: opens `state.db` (integrity, schema version), FTS and
/// quarantine records.
pub(super) fn store(inputs: &Inputs<'_>) -> Vec<Check> {
    match pmt_store::Store::open(inputs.scope) {
        Ok(store) => {
            let mut checks = vec![Check::new(
                "store",
                "schema_version",
                Status::Ok,
                format!(
                    "state.db opens at schema version {}",
                    pmt_store::SCHEMA_VERSION
                ),
            )];
            match store.doctor_checks() {
                Ok(more) => checks.extend(more),
                Err(err) => checks.push(Check::new(
                    "store",
                    "indexes",
                    Status::Fail,
                    err.message().to_owned(),
                )),
            }
            checks
        }
        Err(err) => vec![
            Check::new("store", "open", Status::Fail, err.message().to_owned()).with_hint(
                err.hint()
                    .unwrap_or("prometheus sessions repair --indexes")
                    .to_owned(),
            ),
        ],
    }
}

/// `daemon`: a running daemon must be this build.
pub(super) fn daemon(inputs: &Inputs<'_>) -> Vec<Check> {
    let check = match live_record(inputs.scope.home(), Role::Daemon) {
        Ok(None) => Check::new(
            "daemon",
            "running",
            Status::Ok,
            "not running (sessions run in-process)",
        ),
        Ok(Some(record)) if record.version == crate::build_info::VERSION => Check::new(
            "daemon",
            "running",
            Status::Ok,
            format!("running (pid {}, version {})", record.pid, record.version),
        ),
        Ok(Some(record)) => Check::new(
            "daemon",
            "running",
            Status::Warn,
            format!(
                "running version {} (pid {}), this binary is {}",
                record.version,
                record.pid,
                crate::build_info::VERSION
            ),
        )
        .with_hint("restart the daemon to run this build"),
        Err(err) => Check::new("daemon", "running", Status::Warn, err.message().to_owned()),
    };
    vec![check]
}

/// `runtimes`: managed runtimes (none are installed by this build yet).
pub(super) fn runtimes() -> Vec<Check> {
    vec![Check::new(
        "runtimes",
        "managed",
        Status::Ok,
        "no managed runtimes installed",
    )]
}

/// `plugins`: signatures and grants of installed plugins.
pub(super) fn plugins() -> Vec<Check> {
    vec![Check::new(
        "plugins",
        "installed",
        Status::Ok,
        "no plugins installed",
    )]
}

/// `observability`: exporters (none can be enabled in this build).
pub(super) fn observability(inputs: &Inputs<'_>) -> Vec<Check> {
    let trajectories = inputs.scope.config().observability.trajectories.enabled;
    vec![Check::new(
        "observability",
        "exporters",
        Status::Ok,
        format!(
            "no exporter enabled; trajectories {}",
            if trajectories { "on" } else { "off" }
        ),
    )]
}

/// `providers`: a credential for every configured provider; with
/// `--online`, a TCP connection to its endpoint.
pub(super) async fn providers(inputs: &Inputs<'_>) -> Result<Vec<Check>, Error> {
    let mut checks = Vec::new();
    for secret in required_secrets(inputs.scope)? {
        let (status, state) = if secret.present {
            (Status::Ok, "credential present")
        } else {
            (Status::Fail, "no credential")
        };
        let mut check = Check::new(
            "providers",
            "credential",
            status,
            format!("{}: {state} ({})", secret.provider, secret.name),
        );
        if !secret.present {
            check = check.with_hint(format!("prometheus secrets set {}", secret.name));
        }
        checks.push(check);
    }
    for (provider, host, port) in super::system::provider_endpoints(inputs.scope) {
        checks.push(if inputs.online {
            reach(&provider, &host, port).await
        } else {
            Check::new(
                "providers",
                "reachable",
                Status::Ok,
                format!("{provider}: {host}:{port} not probed (use --online)"),
            )
        });
    }
    Ok(checks)
}

async fn reach(provider: &str, host: &str, port: u16) -> Check {
    let connect = tokio::net::TcpStream::connect((host, port));
    match tokio::time::timeout(PROBE, connect).await {
        Ok(Ok(_)) => Check::new(
            "providers",
            "reachable",
            Status::Ok,
            format!("{provider}: {host}:{port} reachable"),
        ),
        Ok(Err(e)) => Check::new(
            "providers",
            "reachable",
            Status::Warn,
            format!("{provider}: {host}:{port} unreachable ({e})"),
        ),
        Err(_) => Check::new(
            "providers",
            "reachable",
            Status::Warn,
            format!("{provider}: {host}:{port} timed out"),
        ),
    }
}
