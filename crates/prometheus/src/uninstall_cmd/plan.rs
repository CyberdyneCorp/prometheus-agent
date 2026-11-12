//! What one `prometheus uninstall` removes or stops, listed before
//! anything happens (the dry run prints exactly this list).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};

use super::UninstallArgs;
use crate::context::Context;

/// Everything an uninstall touches.
#[derive(Debug, Default)]
pub(super) struct Plan {
    /// The home's running daemon (pid), stopped first.
    daemon: Option<u32>,
    /// Supervisor units `install-service` created.
    units: Vec<String>,
    /// The binary, its alias, `prometheus.prev` and the receipt.
    binary: Vec<PathBuf>,
    /// The home itself (`--purge`).
    home: Option<PathBuf>,
    /// The home's entries (`--data-only`).
    home_contents: Vec<PathBuf>,
    /// The desktop app's per-user data directories.
    desktop: Vec<PathBuf>,
}

impl Plan {
    pub(super) fn build(
        args: &UninstallArgs,
        ctx: &Context<'_>,
        binary: &Path,
        home: &Path,
    ) -> Result<Self, Error> {
        let mut plan = Self::default();
        if args.removes_binary() {
            plan.binary = binary_files(binary);
            plan.units = units(ctx)?;
        }
        if args.removes_binary() || args.removes_home() {
            plan.daemon = running_daemon(home);
        }
        if args.purge && home.exists() {
            plan.home = Some(home.to_owned());
        } else if args.data_only {
            plan.home_contents = entries(home);
        }
        if args.desktop_data {
            plan.desktop = desktop_dirs(ctx)
                .into_iter()
                .filter(|d| d.exists())
                .collect();
        }
        Ok(plan)
    }

    /// Whether the user's data goes (asks first).
    pub(super) fn deletes_data(&self) -> bool {
        self.home.is_some() || !self.home_contents.is_empty() || !self.desktop.is_empty()
    }

    /// One line per item; `planned` words them as what would happen.
    pub(super) fn render(&self, planned: bool) -> String {
        let verb = |text: &str| {
            if planned {
                format!("would {text}")
            } else {
                text.to_owned()
            }
        };
        let mut out = String::new();
        if let Some(pid) = self.daemon {
            let _ = writeln!(out, "{}: the gateway daemon (pid {pid})", verb("stop"));
        }
        for unit in &self.units {
            let _ = writeln!(out, "{} service unit: {unit}", verb("remove"));
        }
        for file in &self.binary {
            let _ = writeln!(out, "{}: {}", verb("remove"), file.display());
        }
        if let Some(home) = &self.home {
            let _ = writeln!(out, "{} the home: {}", verb("remove"), home.display());
        }
        for entry in self.home_contents.iter().chain(&self.desktop) {
            let _ = writeln!(out, "{}: {}", verb("remove"), entry.display());
        }
        if out.is_empty() {
            out.push_str("nothing to remove\n");
        }
        out
    }

    /// Carries the plan out: units, daemon, data, then the binary; returns
    /// notes about what stays (a running Windows executable).
    pub(super) fn execute(&self, ctx: &Context<'_>) -> Result<Vec<String>, Error> {
        for removed in remove_units(ctx, !self.units.is_empty())? {
            println!("{removed}");
        }
        if self.daemon.is_some() {
            stop_daemon(ctx)?;
            println!("stopped the gateway daemon");
        }
        for dir in self
            .home
            .iter()
            .chain(&self.home_contents)
            .chain(&self.desktop)
        {
            remove(dir)?;
            println!("removed {}", dir.display());
        }
        let mut notes = Vec::new();
        for file in &self.binary {
            match remove_binary_file(file)? {
                Some(note) => notes.push(note),
                None => println!("removed {}", file.display()),
            }
        }
        if !self.binary.is_empty() && self.home.is_none() && self.home_contents.is_empty() {
            notes.push(
                "the Prometheus home was kept; `prometheus uninstall --purge` removes it"
                    .to_owned(),
            );
        }
        Ok(notes)
    }
}

/// The binary, its `pmt` alias, `prometheus.prev` and `install.json`.
fn binary_files(binary: &Path) -> Vec<PathBuf> {
    let exe = std::env::consts::EXE_SUFFIX;
    let candidates = [
        binary.to_owned(),
        binary.with_file_name(format!("pmt{exe}")),
        pmt_update::swap::prev_path(binary),
        pmt_update::install::beside(binary),
    ];
    candidates
        .into_iter()
        .filter(|p| p.symlink_metadata().is_ok())
        .collect()
}

fn entries(home: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(home)
        .map(|dir| dir.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    entries.sort();
    entries
}

/// Where the desktop app keeps per-user data (webview storage, window
/// state, logs) on this OS.
fn desktop_dirs(ctx: &Context<'_>) -> Vec<PathBuf> {
    const APP: &str = "Prometheus Desktop";
    let Some(user) = ctx.launch.user_home() else {
        return Vec::new();
    };
    if cfg!(target_os = "macos") {
        [
            "Library/Application Support",
            "Library/Caches",
            "Library/Logs",
            "Library/WebKit",
        ]
        .iter()
        .map(|base| user.join(base).join(APP))
        .collect()
    } else if cfg!(windows) {
        ["AppData/Roaming", "AppData/Local"]
            .iter()
            .map(|base| user.join(base).join(APP))
            .collect()
    } else {
        [".config", ".local/share", ".cache", ".local/state"]
            .iter()
            .map(|base| user.join(base).join("prometheus-desktop"))
            .collect()
    }
}

fn remove(path: &Path) -> Result<(), Error> {
    let removed = if path.is_dir() && !path.is_symlink() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match removed {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io_doing(&codes::HOME_IO, "cannot remove", path, &e)),
    }
}

/// Removes one binary file; on Windows the running executable cannot be
/// deleted, so it is renamed aside and a note says to delete it later.
fn remove_binary_file(file: &Path) -> Result<Option<String>, Error> {
    match remove(file) {
        Ok(()) => Ok(None),
        Err(_) if cfg!(windows) => {
            let aside = file.with_extension("exe.uninstalled");
            std::fs::rename(file, &aside)
                .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot remove", file, &e))?;
            Ok(Some(format!(
                "{} is still running; delete {} once this command has exited",
                file.display(),
                aside.display()
            )))
        }
        Err(err) => Err(err),
    }
}

#[cfg(feature = "gateway")]
fn units(ctx: &Context<'_>) -> Result<Vec<String>, Error> {
    crate::daemon_cmd::service::installed_units(ctx)
}

#[cfg(not(feature = "gateway"))]
fn units(_: &Context<'_>) -> Result<Vec<String>, Error> {
    Ok(Vec::new())
}

#[cfg(feature = "gateway")]
fn remove_units(ctx: &Context<'_>, any: bool) -> Result<Vec<String>, Error> {
    if any {
        crate::daemon_cmd::service::remove_units(ctx)
    } else {
        Ok(Vec::new())
    }
}

#[cfg(not(feature = "gateway"))]
fn remove_units(_: &Context<'_>, _: bool) -> Result<Vec<String>, Error> {
    Ok(Vec::new())
}

fn running_daemon(home: &Path) -> Option<u32> {
    match pmt_rpc::discovery::discover(home) {
        Ok(pmt_rpc::discovery::Discovery::Running(record)) => Some(record.process.pid),
        Ok(pmt_rpc::discovery::Discovery::Starting(record)) => Some(record.pid),
        _ => None,
    }
}

#[cfg(feature = "gateway")]
fn stop_daemon(ctx: &Context<'_>) -> Result<(), Error> {
    let home = ctx.home()?.root().to_path_buf();
    crate::daemon_cmd::stop_and_wait(&home, false, crate::daemon_cmd::drain_wait(&home)).map(drop)
}

#[cfg(not(feature = "gateway"))]
fn stop_daemon(_: &Context<'_>) -> Result<(), Error> {
    Ok(())
}
