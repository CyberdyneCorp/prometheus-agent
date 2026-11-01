//! Where the Prometheus home is: `--home`, else a non-blank
//! `PROMETHEUS_HOME` (with `~` expanded), else `~/.prometheus` on macOS and
//! Linux and `%LOCALAPPDATA%\Prometheus` on Windows
//! (spec: configuration-and-profiles, Home resolution). It is resolved once
//! at startup from the launch environment; creating the layout is the
//! caller's job.

use std::path::{Path, PathBuf};

use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};

/// Resolves the home from the `--home` flag and the launch environment.
///
/// # Errors
///
/// `home.unresolved` (exit 78) when neither is set and the user home (or
/// `LOCALAPPDATA` on Windows) is unknown, or `~` cannot be expanded.
pub fn resolve(flag: Option<&Path>, env: &LaunchEnv) -> Result<PathBuf, Error> {
    if let Some(path) = flag.or(env.home().map(PathBuf::as_path)) {
        return expand_tilde(path, env);
    }
    default_home(env).ok_or_else(|| {
        Error::new(
            &codes::HOME_UNRESOLVED,
            "cannot determine the Prometheus home: the user home directory is unknown",
        )
        .with_hint("pass --home PATH or set PROMETHEUS_HOME")
    })
}

#[cfg(not(windows))]
fn default_home(env: &LaunchEnv) -> Option<PathBuf> {
    env.user_home().map(|home| home.join(".prometheus"))
}

#[cfg(windows)]
fn default_home(env: &LaunchEnv) -> Option<PathBuf> {
    env.local_app_data().map(|dir| dir.join("Prometheus"))
}

/// `~` and `~/rest` (or `~\rest`) become the user home; anything else is
/// returned unchanged.
fn expand_tilde(path: &Path, env: &LaunchEnv) -> Result<PathBuf, Error> {
    let Ok(rest) = path.strip_prefix("~") else {
        return Ok(path.to_owned());
    };
    let user_home = env.user_home().ok_or_else(|| {
        Error::new(
            &codes::HOME_UNRESOLVED,
            format!(
                "cannot expand '~' in {}: the user home directory is unknown",
                path.display()
            ),
        )
        .with_hint("use an absolute path")
    })?;
    Ok(user_home.join(rest))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use pmt_core::env::{EnvPolicy, LaunchEnv};

    use super::resolve;

    fn env(vars: &[(&str, &str)]) -> LaunchEnv {
        let vars = vars
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        LaunchEnv::from_vars(vars, &EnvPolicy::default())
    }

    fn user_home_var() -> &'static str {
        if cfg!(windows) { "USERPROFILE" } else { "HOME" }
    }

    #[test]
    fn flag_beats_environment() {
        let env = env(&[("PROMETHEUS_HOME", "/from/env")]);
        let home = resolve(Some(Path::new("/from/flag")), &env).unwrap();
        assert_eq!(home, PathBuf::from("/from/flag"));
    }

    #[test]
    fn environment_with_tilde_is_expanded() {
        let env = env(&[
            ("PROMETHEUS_HOME", "~/agent"),
            (user_home_var(), "/users/me"),
        ]);
        assert_eq!(
            resolve(None, &env).unwrap(),
            Path::new("/users/me").join("agent")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn default_is_dot_prometheus_in_the_user_home() {
        let env = env(&[("HOME", "/users/me")]);
        assert_eq!(
            resolve(None, &env).unwrap(),
            PathBuf::from("/users/me/.prometheus")
        );
    }

    #[cfg(windows)]
    #[test]
    fn default_is_prometheus_in_local_app_data() {
        let env = env(&[("LOCALAPPDATA", r"C:\Users\me\AppData\Local")]);
        assert_eq!(
            resolve(None, &env).unwrap(),
            PathBuf::from(r"C:\Users\me\AppData\Local\Prometheus")
        );
    }

    #[test]
    fn unknown_user_home_is_a_config_error() {
        let err = resolve(None, &env(&[])).unwrap_err();
        assert_eq!(err.exit_code(), 78);
        assert_eq!(err.code().code, "home.unresolved");
    }
}
