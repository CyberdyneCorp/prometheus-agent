//! Whole-config checks of the messaging gateway keys (spec:
//! messaging-gateway, Authorization order and fail-closed default; Gateway
//! slash commands, quick commands and hooks): an allowlist never holds a
//! wildcard (admitting everyone is the explicit `allow_all`), and a quick
//! command never shadows a command of the shared registry.

use std::sync::OnceLock;

use pmt_core::error::{Error, codes};

use crate::schema::Config;

/// The shared slash command registry's names and aliases, set once by the
/// binary that owns the registry. Unset (libraries, tests): no quick
/// command name is reserved.
static RESERVED: OnceLock<Vec<String>> = OnceLock::new();

/// Reserves the registry's command names: a quick command named like one
/// fails validation from then on. The first call wins.
pub fn reserve_command_names(names: impl IntoIterator<Item = impl Into<String>>) {
    let _ = RESERVED.set(names.into_iter().map(Into::into).collect());
}

/// Refuses a wildcard allowlist entry and a quick command that collides
/// with a registry command.
///
/// # Errors
///
/// `config.invalid` naming the key.
pub fn check(config: &Config) -> Result<(), Error> {
    for (platform, table) in &config.gateway.platforms {
        if table.allow_users.iter().any(|user| user.contains('*')) {
            return Err(Error::new(
                &codes::CONFIG_INVALID,
                format!(
                    "gateway.platforms.{platform}.allow_users: wildcards are not allowed; set gateway.platforms.{platform}.allow_all = true to admit everyone"
                ),
            ));
        }
    }
    let reserved = RESERVED.get().map(Vec::as_slice).unwrap_or_default();
    for name in config.quick_commands.keys() {
        if reserved.iter().any(|r| r == name) {
            return Err(Error::new(
                &codes::CONFIG_INVALID,
                format!(
                    "quick_commands.{name}: /{name} is a built-in command; give the quick command another name"
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::PlatformConfig;

    #[test]
    fn a_wildcard_allowlist_points_to_allow_all() {
        let mut config = Config::default();
        config.gateway.platforms.insert(
            "telegram".into(),
            PlatformConfig {
                allow_users: vec!["*".into()],
                ..PlatformConfig::default()
            },
        );
        let err = check(&config).unwrap_err();
        assert_eq!(err.code(), &codes::CONFIG_INVALID);
        assert!(
            err.message()
                .contains("gateway.platforms.telegram.allow_users")
        );
        assert!(err.message().contains("allow_all"), "{}", err.message());
    }
}
