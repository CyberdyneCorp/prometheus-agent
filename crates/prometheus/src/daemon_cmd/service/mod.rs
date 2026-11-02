//! `prometheus daemon install-service` and `uninstall-service` (spec:
//! messaging-gateway, Per-OS service installation without elevation): a
//! per-user supervisor unit that starts the daemon at login without
//! administrator rights (systemd `--user` unit with lingering when
//! allowed, `LaunchAgent`, Scheduled Task `ONLOGON /RL LIMITED` with a
//! Startup-folder fallback), or with `--system --run-as-user` a boot-time
//! unit that runs as that user (root only). Re-installing rewrites the
//! same unit, so exactly one exists; uninstalling a unit that is not there
//! succeeds.
//!
//! Debug builds with `PMT_TEST_SERVICE_DIR` write every unit under that
//! directory and record the service-manager commands in its
//! `commands.log` instead of running them (`refuse-<program>` makes that
//! program fail, `root` makes the caller root), so the flow is testable.

mod units;

use std::path::{Path, PathBuf};

use clap::Args;
use pmt_core::error::{Error, codes};

use crate::context::Context;
use units::Daemon;
#[cfg(target_os = "macos")]
use units::LAUNCHD_LABEL;
#[cfg(target_os = "linux")]
use units::SYSTEMD_UNIT;
#[cfg(windows)]
use units::TASK_NAME;

/// `daemon install-service` arguments.
#[derive(Debug, Clone, Args)]
pub struct InstallArgs {
    /// Install a boot-time system unit instead (root or administrator).
    #[arg(long, requires = "run_as_user")]
    pub system: bool,
    /// The user the system unit runs the daemon as.
    #[arg(long, value_name = "USER", requires = "system")]
    pub run_as_user: Option<String>,
    /// Install without starting the daemon now.
    #[arg(long)]
    pub no_start: bool,
}

/// `daemon uninstall-service` arguments.
#[derive(Debug, Clone, Args)]
pub struct UninstallArgs {
    /// Remove the system unit instead of the per-user one.
    #[arg(long)]
    pub system: bool,
}

/// Runs the OS service manager, or records what it would run.
enum Runner {
    Real,
    Recorded(PathBuf),
}

impl Runner {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        match self {
            Self::Real => {
                let output = std::process::Command::new(program)
                    .args(args)
                    .output()
                    .map_err(|e| format!("cannot run {program}: {e}"))?;
                let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_owned();
                if output.status.success() {
                    Ok(text(&output.stdout))
                } else {
                    let stderr = text(&output.stderr);
                    Err(if stderr.is_empty() {
                        text(&output.stdout)
                    } else {
                        stderr
                    })
                }
            }
            Self::Recorded(dir) => {
                let line = format!("{program} {}\n", args.join(" "));
                let log = dir.join("commands.log");
                let previous = std::fs::read_to_string(&log).unwrap_or_default();
                let _ = std::fs::write(&log, previous + &line);
                if dir.join(format!("refuse-{program}")).exists() {
                    Err("Access is denied.".to_owned())
                } else {
                    Ok(String::new())
                }
            }
        }
    }
}

/// Where units go and how the manager is reached.
struct Host {
    runner: Runner,
    /// The user's home directory.
    user_home: PathBuf,
    /// Prefix of system paths (`/` for real).
    system_root: PathBuf,
    /// The Prometheus home.
    home: PathBuf,
    /// The home is the default one (no `PROMETHEUS_HOME` in the unit).
    default_home: bool,
    exe: PathBuf,
}

impl Host {
    fn new(ctx: &Context<'_>) -> Result<Self, Error> {
        let home = ctx.home()?.root().to_path_buf();
        let user_home = ctx.launch.user_home().cloned().ok_or_else(|| {
            Error::new(
                &codes::HOME_UNRESOLVED,
                "the user's home directory is unknown",
            )
        })?;
        let default_home = pmt_config::home::default_home(ctx.launch).is_some_and(|d| d == home);
        let exe = std::env::current_exe().map_err(|e| {
            Error::new(
                &codes::SERVICE_MANAGER,
                format!("cannot find this binary: {e}"),
            )
        })?;
        let (runner, user_home, system_root) = match test_dir(ctx) {
            Some(dir) => (
                Runner::Recorded(dir.clone()),
                dir.join("home"),
                dir.join("system"),
            ),
            None => (Runner::Real, user_home, PathBuf::from("/")),
        };
        Ok(Self {
            runner,
            user_home,
            system_root,
            home,
            default_home,
            exe,
        })
    }

    fn daemon<'a>(&'a self, run_as: Option<&'a str>) -> Daemon<'a> {
        Daemon {
            exe: &self.exe,
            home: (!self.default_home).then_some(self.home.as_path()),
            run_as,
        }
    }

    fn is_root(&self) -> bool {
        match &self.runner {
            Runner::Recorded(dir) => dir.join("root").exists(),
            Runner::Real => is_root(&self.runner),
        }
    }

    fn manager(&self, program: &str, args: &[&str]) -> Result<String, Error> {
        self.runner.run(program, args).map_err(|why| {
            Error::new(
                &codes::SERVICE_MANAGER,
                format!("{program} {} failed: {why}", args.join(" ")),
            )
        })
    }
}

#[cfg(debug_assertions)]
fn test_dir(ctx: &Context<'_>) -> Option<PathBuf> {
    ctx.launch.test_service_dir().cloned()
}

#[cfg(not(debug_assertions))]
fn test_dir(_: &Context<'_>) -> Option<PathBuf> {
    None
}

#[cfg(unix)]
fn is_root(_: &Runner) -> bool {
    nix::unistd::geteuid().is_root()
}

/// `net session` succeeds only in an elevated (administrator) shell.
#[cfg(windows)]
fn is_root(runner: &Runner) -> bool {
    runner.run("net", &["session"]).is_ok()
}

fn write(path: &Path, text: &str) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(&codes::OUTPUT_IO, parent, &e))?;
    }
    std::fs::write(path, text).map_err(|e| Error::io(&codes::OUTPUT_IO, path, &e))
}

fn remove(path: &Path) -> Result<bool, Error> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::io(&codes::OUTPUT_IO, path, &e)),
    }
}

/// Runs `prometheus daemon install-service`.
///
/// # Errors
///
/// `service.needs_root` (77) for `--system` without root,
/// `service.manager` (69) when the service manager refuses.
pub(super) fn install(args: &InstallArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let host = Host::new(ctx)?;
    if args.system && !host.is_root() {
        return Err(Error::new(
            &codes::SERVICE_NEEDS_ROOT,
            "system service install requires root",
        )
        .with_hint(
            "run it with sudo (as administrator on Windows), or drop --system for a per-user unit",
        ));
    }
    let run_as = args.run_as_user.as_deref();
    let done = install_on_os(&host, run_as, !args.no_start)?;
    crate::output::print(&done)
}

/// Runs `prometheus daemon uninstall-service`.
///
/// # Errors
///
/// `service.needs_root` (77) for `--system` without root; file errors.
pub(super) fn uninstall(args: &UninstallArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let host = Host::new(ctx)?;
    if args.system && !host.is_root() {
        return Err(Error::new(
            &codes::SERVICE_NEEDS_ROOT,
            "system service removal requires root",
        ));
    }
    let done = uninstall_on_os(&host, args.system)?;
    crate::output::print(&done)
}

#[cfg(target_os = "linux")]
fn install_on_os(host: &Host, run_as: Option<&str>, start: bool) -> Result<String, Error> {
    let system = run_as.is_some();
    let path = systemd_path(host, system);
    write(&path, &units::systemd(&host.daemon(run_as)))?;
    let scope: &[&str] = if system { &[] } else { &["--user"] };
    let with = |rest: &[&str]| [scope, rest].concat();
    host.manager("systemctl", &with(&["daemon-reload"]))?;
    let enable: &[&str] = if start {
        &["enable", "--now", SYSTEMD_UNIT]
    } else {
        &["enable", SYSTEMD_UNIT]
    };
    host.manager("systemctl", &with(enable))?;
    let mut note = String::new();
    if !system {
        // Lingering keeps the user's daemon alive after logout; a refusal
        // (policy) only means it stops with the session.
        if let Err(why) = host.runner.run("loginctl", &["enable-linger"]) {
            note = format!("\nlingering not enabled ({why}); the daemon stops when you log out");
        }
    }
    Ok(format!(
        "installed {} ({}){note}",
        path.display(),
        if start { "started" } else { "not started" }
    ))
}

#[cfg(target_os = "linux")]
fn systemd_path(host: &Host, system: bool) -> PathBuf {
    if system {
        host.system_root
            .join("etc/systemd/system")
            .join(SYSTEMD_UNIT)
    } else {
        host.user_home
            .join(".config/systemd/user")
            .join(SYSTEMD_UNIT)
    }
}

#[cfg(target_os = "linux")]
fn uninstall_on_os(host: &Host, system: bool) -> Result<String, Error> {
    let path = systemd_path(host, system);
    if !path.exists() {
        return Ok("no service installed".to_owned());
    }
    let scope: &[&str] = if system { &[] } else { &["--user"] };
    // Stopping a unit that is not running is not an error worth stopping for.
    let _ = host.runner.run(
        "systemctl",
        &[scope, &["disable", "--now", SYSTEMD_UNIT]].concat(),
    );
    remove(&path)?;
    host.manager("systemctl", &[scope, &["daemon-reload"]].concat())?;
    Ok(format!("removed {}", path.display()))
}

#[cfg(target_os = "macos")]
fn launchd_target(host: &Host, system: bool) -> (PathBuf, String) {
    if system {
        (
            host.system_root
                .join("Library/LaunchDaemons")
                .join(format!("{LAUNCHD_LABEL}.plist")),
            "system".to_owned(),
        )
    } else {
        (
            host.user_home
                .join("Library/LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist")),
            format!("gui/{}", nix::unistd::getuid()),
        )
    }
}

#[cfg(target_os = "macos")]
fn install_on_os(host: &Host, run_as: Option<&str>, start: bool) -> Result<String, Error> {
    let (path, domain) = launchd_target(host, run_as.is_some());
    let stderr = host.home.join("logs").join("daemon-stderr.log");
    write(&path, &units::launchd(&host.daemon(run_as), &stderr))?;
    let service = format!("{domain}/{LAUNCHD_LABEL}");
    // Unload an earlier copy first, so exactly one stays loaded.
    let _ = host.runner.run("launchctl", &["bootout", &service]);
    if start {
        host.manager(
            "launchctl",
            &["bootstrap", &domain, &path.to_string_lossy()],
        )?;
    }
    Ok(format!(
        "installed {} ({})",
        path.display(),
        if start {
            "loaded and started"
        } else {
            "not started: it loads at the next login"
        }
    ))
}

#[cfg(target_os = "macos")]
fn uninstall_on_os(host: &Host, system: bool) -> Result<String, Error> {
    let (path, domain) = launchd_target(host, system);
    if !path.exists() {
        return Ok("no service installed".to_owned());
    }
    let _ = host.runner.run(
        "launchctl",
        &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")],
    );
    remove(&path)?;
    Ok(format!("removed {}", path.display()))
}

#[cfg(windows)]
fn windows_paths(host: &Host) -> (PathBuf, PathBuf) {
    let wrapper = host.home.join("run").join("prometheus-daemon.cmd");
    let startup = host
        .user_home
        .join(r"AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup")
        .join(format!("{TASK_NAME}.cmd"));
    (wrapper, startup)
}

#[cfg(windows)]
fn install_on_os(host: &Host, run_as: Option<&str>, start: bool) -> Result<String, Error> {
    let daemon = host.daemon(run_as);
    let (wrapper, startup) = windows_paths(host);
    write(&wrapper, &units::windows_wrapper(&daemon))?;
    let action = format!("cmd.exe /c \"{}\"", wrapper.display());
    let mut create = vec![
        "/Create",
        "/TN",
        TASK_NAME,
        "/TR",
        action.as_str(),
        "/RL",
        "LIMITED",
        "/F",
    ];
    match run_as {
        Some(user) => create.extend(["/SC", "ONSTART", "/RU", user, "/NP"]),
        None => create.extend(["/SC", "ONLOGON"]),
    }
    match host.runner.run("schtasks", &create) {
        Ok(_) => {
            remove(&startup)?;
            if start {
                host.manager("schtasks", &["/Run", "/TN", TASK_NAME])?;
            }
            Ok(format!(
                "installed the Scheduled Task '{TASK_NAME}' ({})",
                if start { "started" } else { "not started" }
            ))
        }
        Err(why) if run_as.is_none() => {
            write(&startup, &units::windows_startup(&daemon))?;
            if start {
                host.manager(&host.exe.to_string_lossy(), &["daemon", "start"])?;
            }
            Ok(format!(
                "Task Scheduler refused ({why}); used the fallback: {} runs 'prometheus daemon start' at logon",
                startup.display()
            ))
        }
        Err(why) => Err(Error::new(
            &codes::SERVICE_MANAGER,
            format!("schtasks failed: {why}"),
        )),
    }
}

#[cfg(windows)]
fn uninstall_on_os(host: &Host, _system: bool) -> Result<String, Error> {
    let (wrapper, startup) = windows_paths(host);
    let task = host
        .runner
        .run("schtasks", &["/Query", "/TN", TASK_NAME])
        .is_ok();
    if task {
        let _ = host.runner.run("schtasks", &["/End", "/TN", TASK_NAME]);
        host.manager("schtasks", &["/Delete", "/TN", TASK_NAME, "/F"])?;
    }
    let fallback = remove(&startup)?;
    remove(&wrapper)?;
    Ok(if task || fallback {
        format!("removed the '{TASK_NAME}' service")
    } else {
        "no service installed".to_owned()
    })
}

/// A unit left by an earlier layout or an imported Hermes gateway, which
/// `prometheus doctor` reports with the command that replaces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyUnit {
    /// Where it is.
    pub path: PathBuf,
    /// What installed it.
    pub origin: &'static str,
}

/// Legacy supervisor units of the current user.
#[must_use]
pub fn legacy_units(user_home: &Path) -> Vec<LegacyUnit> {
    let candidates: &[(&str, &str)] = &[
        (
            ".config/systemd/user/hermes-gateway.service",
            "Hermes gateway",
        ),
        (
            ".config/systemd/user/prometheus-gateway.service",
            "an earlier Prometheus layout",
        ),
        (
            "Library/LaunchAgents/ai.hermes.gateway.plist",
            "Hermes gateway",
        ),
        (
            "Library/LaunchAgents/ai.prometheus.gateway.plist",
            "an earlier Prometheus layout",
        ),
    ];
    candidates
        .iter()
        .map(|(relative, origin)| LegacyUnit {
            path: user_home.join(relative),
            origin,
        })
        .filter(|unit| unit.path.exists())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermes_and_old_units_are_found() {
        let home = tempfile::tempdir().unwrap();
        assert!(legacy_units(home.path()).is_empty());
        let unit = home
            .path()
            .join(".config/systemd/user/hermes-gateway.service");
        std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
        std::fs::write(&unit, "[Unit]\n").unwrap();
        let found = legacy_units(home.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].origin, "Hermes gateway");
    }
}
