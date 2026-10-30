//! Validation of the non-secret environment passthrough (spec:
//! execution-environments, Non-secret environment passthrough).
//!
//! `terminal.env_passthrough` copies launch-environment variables into every
//! child and `terminal.container.env` sets literal ones. Neither may carry a
//! secret: secrets reach children only through a `secrets.read:<NAME>` grant
//! the target declares, so they stay auditable and revocable.

use pmt_core::env::SECRET_PREFIX;
use pmt_core::error::{Error, codes};

use crate::schema::TerminalConfig;
use crate::secrets::SecretsHandle;

/// Name suffixes that mark a variable as a credential even when no store or
/// provider knows it (`OPENAI_API_KEY`, `GITHUB_TOKEN`, `DB_PASSWORD`...).
const SECRET_SUFFIXES: &[&str] = &[
    "_KEY",
    "_TOKEN",
    "_SECRET",
    "_PASSWORD",
    "_PASSWD",
    "_PASSPHRASE",
    "_CREDENTIAL",
    "_CREDENTIALS",
];

/// Whole names that are credentials on their own.
const SECRET_NAMES: &[&str] = &["API_KEY", "TOKEN", "SECRET", "PASSWORD", "PASSPHRASE"];

/// Whether `name` looks like a secret on its own: the `PMT_SECRET_*`
/// pattern or a credential-shaped name (case-insensitive).
#[must_use]
pub fn looks_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.starts_with(SECRET_PREFIX)
        || SECRET_NAMES.contains(&upper.as_str())
        || SECRET_SUFFIXES.iter().any(|suffix| upper.ends_with(suffix))
}

/// Checks `terminal.env_passthrough` and `terminal.container.env` against
/// `is_known_secret` (stored secret names, provider-native credential names
/// and launch-environment secrets) and [`looks_secret`].
///
/// # Errors
///
/// `terminal.env_passthrough_secret` (exit 78) for the first offending name:
/// `terminal.env_passthrough: '<NAME>' is a secret; grant it with
/// secrets.read instead`.
pub fn check(
    terminal: &TerminalConfig,
    is_known_secret: &dyn Fn(&str) -> bool,
) -> Result<(), Error> {
    // Names may also be refused for their shape; literal values only when
    // they name a secret (a value like `/srv/server_key` is not one).
    let secret_name = |name: &str| looks_secret(name) || is_known_secret(name);
    let secret_value = |value: &str| value.starts_with(SECRET_PREFIX) || is_known_secret(value);
    let offending = terminal
        .env_passthrough
        .iter()
        .find(|name| secret_name(name))
        .map(|name| ("terminal.env_passthrough", name))
        .or_else(|| {
            terminal.container.env.iter().find_map(|(name, value)| {
                let hit = if secret_name(name) {
                    Some(name)
                } else {
                    secret_value(value).then_some(value)
                };
                hit.map(|hit| ("terminal.container.env", hit))
            })
        });
    match offending {
        None => Ok(()),
        Some((key, name)) => Err(Error::new(
            &codes::TERMINAL_ENV_PASSTHROUGH_SECRET,
            format!("{key}: '{name}' is a secret; grant it with secrets.read instead"),
        )
        .with_hint(format!(
            "remove '{name}' from {key} and grant the target secrets.read:{name}"
        ))),
    }
}

/// [`check`] against the profile's secrets: stored names and the launch
/// environment's secret variables. The store is only read when a
/// passthrough is configured.
///
/// # Errors
///
/// As [`check`], plus secret store errors.
pub fn check_with_secrets(terminal: &TerminalConfig, secrets: &SecretsHandle) -> Result<(), Error> {
    if terminal.env_passthrough.is_empty() && terminal.container.env.is_empty() {
        return Ok(());
    }
    let stored = secrets.store().names()?;
    let known = |name: &str| {
        stored.iter().any(|s| s.eq_ignore_ascii_case(name))
            || secrets.launch_env_secret(name).is_some()
    };
    check(terminal, &known)
}

#[cfg(test)]
mod tests {
    use super::{check, looks_secret};
    use crate::schema::TerminalConfig;

    fn passthrough(names: &[&str]) -> TerminalConfig {
        TerminalConfig {
            env_passthrough: names.iter().map(|n| (*n).to_owned()).collect(),
            ..TerminalConfig::default()
        }
    }

    #[test]
    fn toolchain_variables_pass() {
        let config = passthrough(&["JAVA_HOME", "GOPATH", "SSH_AUTH_SOCK", "NODE_OPTIONS"]);
        assert!(check(&config, &|_| false).is_ok());
    }

    #[test]
    fn secret_looking_and_known_names_are_refused() {
        for name in [
            "OPENAI_API_KEY",
            "PMT_SECRET_X",
            "GITHUB_TOKEN",
            "db_password",
        ] {
            let err = check(&passthrough(&[name]), &|_| false).unwrap_err();
            assert_eq!(err.exit_code(), 78);
            assert_eq!(
                err.message(),
                format!(
                    "terminal.env_passthrough: '{name}' is a secret; grant it with secrets.read instead"
                )
            );
        }
        let stored = |name: &str| name == "ACME";
        assert!(check(&passthrough(&["ACME"]), &stored).is_err());
        assert!(looks_secret("API_KEY") && !looks_secret("KEYBOARD_LAYOUT"));
    }

    #[test]
    fn container_env_names_and_values_are_checked() {
        let mut config = TerminalConfig::default();
        config
            .container
            .env
            .insert("KEY_FILE".into(), "/srv/server_key".into());
        assert!(check(&config, &|_| false).is_ok());
        config
            .container
            .env
            .insert("MODE".into(), "PMT_SECRET_X".into());
        let err = check(&config, &|_| false).unwrap_err();
        assert!(
            err.message()
                .starts_with("terminal.container.env: 'PMT_SECRET_X'")
        );
        config.container.env.clear();
        config
            .container
            .env
            .insert("GITHUB_TOKEN".into(), "x".into());
        assert!(check(&config, &|_| false).is_err());
    }
}
