//! Profile alias launchers (spec: configuration-and-profiles, Profile alias
//! launchers): `profile alias <name> [--as <alias>]` writes a launcher
//! `<alias>` that runs `prometheus -p <name> "$@"` into the user's bin
//! directory (`~/.local/bin`, or `%LOCALAPPDATA%\Prometheus\bin`), adding
//! `--home <home>` when the profile lives outside the default home, and
//! refuses an alias that is reserved, a `prometheus` subcommand, or an
//! existing executable on `PATH`. A launcher this command wrote earlier is
//! recognised by its marker line and may be rewritten.

use std::path::{Path, PathBuf};

use clap::CommandFactory as _;
use pmt_config::ProfileName;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};

/// The line that marks a launcher as ours.
const MARKER: &str = "prometheus profile alias";

/// Where launchers go.
///
/// # Errors
///
/// `profile.alias_refused` when the user's home is unknown.
pub(super) fn bin_dir(launch: &LaunchEnv) -> Result<PathBuf, Error> {
    let dir = if cfg!(windows) {
        launch
            .local_app_data()
            .map(|d| d.join("Prometheus").join("bin"))
    } else {
        launch.user_home().map(|h| h.join(".local").join("bin"))
    };
    dir.ok_or_else(|| refused("cannot find your home directory for the launcher".into()))
}

fn refused(message: String) -> Error {
    Error::new(&codes::PROFILE_ALIAS_REFUSED, message)
}

/// The launcher file for `alias` in `dir`.
fn launcher_path(dir: &Path, alias: &str) -> PathBuf {
    if cfg!(windows) {
        dir.join(format!("{alias}.cmd"))
    } else {
        dir.join(alias)
    }
}

/// Whether `path` is a launcher this command wrote.
fn is_ours(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| text.contains(MARKER))
}

/// Fails when `alias` may not be used: a bad or reserved name, a
/// subcommand, or an executable on `PATH` (or a file in the bin directory)
/// that is not one of our launchers.
///
/// # Errors
///
/// `profile.alias_refused` (2) naming the reason.
pub(super) fn check(alias: &str, dir: &Path, launch: &LaunchEnv) -> Result<(), Error> {
    let valid = ProfileName::parse(alias).is_ok_and(|n| n.as_str() == alias);
    if !valid {
        return Err(refused(format!(
            "alias '{alias}' is not a valid name (lowercase letters, digits, '_' and '-')"
        )));
    }
    if pmt_config::layout::RESERVED_NAMES.contains(&alias) {
        return Err(refused(format!("alias '{alias}' is reserved")));
    }
    let command = crate::cli::Cli::command();
    let subcommand = command
        .get_subcommands()
        .any(|c| c.get_name() == alias || c.get_all_aliases().any(|a| a == alias));
    if subcommand {
        return Err(refused(format!(
            "alias '{alias}' is a prometheus subcommand"
        )));
    }
    let own = launcher_path(dir, alias);
    if own.exists() && !is_ours(&own) {
        return Err(refused(format!(
            "alias '{alias}' would overwrite {}",
            own.display()
        )));
    }
    if let Some(found) = on_path(alias, launch).filter(|p| !is_ours(p)) {
        return Err(refused(format!(
            "alias '{alias}' would shadow the existing executable {}",
            found.display()
        )));
    }
    Ok(())
}

/// The first executable named `name` on the launch environment's `PATH`.
fn on_path(name: &str, launch: &LaunchEnv) -> Option<PathBuf> {
    let path = launch.inherited("PATH")?;
    let names: Vec<String> = if cfg!(windows) {
        ["exe", "cmd", "bat", "com"]
            .iter()
            .map(|ext| format!("{name}.{ext}"))
            .collect()
    } else {
        vec![name.to_owned()]
    };
    std::env::split_paths(path)
        .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Writes the launcher for `profile` as `alias` into `dir`; returns its
/// path. `home` is the profile's home when it is not the default one: the
/// launcher must open that home, not `~/.prometheus`.
///
/// # Errors
///
/// `home.io` when the directory or file cannot be written.
pub(super) fn write(
    profile: &ProfileName,
    alias: &str,
    dir: &Path,
    home: Option<&Path>,
) -> Result<PathBuf, Error> {
    std::fs::create_dir_all(dir).map_err(|e| Error::io(&codes::HOME_IO, dir, &e))?;
    let path = launcher_path(dir, alias);
    let text = if cfg!(windows) {
        // `%` is expanded in batch files even inside quotes.
        let home = home.map_or_else(String::new, |h| {
            format!("--home \"{}\" ", h.display().to_string().replace('%', "%%"))
        });
        format!(
            "@rem {MARKER}: runs prometheus with profile '{profile}'\r\n@prometheus {home}-p {profile} %*\r\n"
        )
    } else {
        let home = home.map_or_else(String::new, |h| {
            format!(
                "--home '{}' ",
                h.display().to_string().replace('\'', "'\\''")
            )
        });
        format!(
            "#!/bin/sh\n# {MARKER}: runs prometheus with profile '{profile}'\nexec prometheus {home}-p {profile} \"$@\"\n"
        )
    };
    std::fs::write(&path, text).map_err(|e| Error::io(&codes::HOME_IO, &path, &e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| Error::io(&codes::HOME_IO, &path, &e))?;
    }
    Ok(path)
}

/// `profile alias`: checks and writes, then says where (and whether the
/// directory is on `PATH`). `home` is the resolved home of the profile.
///
/// # Errors
///
/// As [`check`] and [`write`].
pub(super) fn run(
    profile: &ProfileName,
    alias: &str,
    home: &Path,
    launch: &LaunchEnv,
) -> Result<PathBuf, Error> {
    let dir = bin_dir(launch)?;
    check(alias, &dir, launch)?;
    write(profile, alias, &dir, custom_home(home, launch).as_deref())
}

/// `home` as an absolute path when it is not the default home.
fn custom_home(home: &Path, launch: &LaunchEnv) -> Option<PathBuf> {
    let absolute = std::path::absolute(home).unwrap_or_else(|_| home.to_owned());
    let default =
        pmt_config::home::default_home(launch).map(|d| std::path::absolute(&d).unwrap_or(d));
    (default.as_deref() != Some(absolute.as_path())).then_some(absolute)
}

/// Whether `dir` is on the launch environment's `PATH`.
pub(super) fn dir_on_path(dir: &Path, launch: &LaunchEnv) -> bool {
    launch
        .inherited("PATH")
        .is_some_and(|path| std::env::split_paths(path).any(|d| d == dir))
}
