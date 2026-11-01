//! Whole-config checks of `[quick_commands]` (spec: cli, Quick commands):
//! an `alias` needs a `target` and must not resolve back to itself
//! through other aliases; an `exec` needs a `command`.

use pmt_core::error::{Error, codes};

use crate::schema::{Config, QuickCommand, QuickCommandKind};

/// Refuses an incomplete quick command or an alias cycle.
///
/// # Errors
///
/// `config.invalid` naming the entry.
pub fn check(config: &Config) -> Result<(), Error> {
    for (name, entry) in &config.quick_commands {
        let key = format!("quick_commands.{name}");
        match entry.kind {
            QuickCommandKind::Alias if entry.target.is_none() => {
                return Err(invalid(&key, "an alias needs `target`"));
            }
            QuickCommandKind::Exec if entry.command.as_deref().is_none_or(str::is_empty) => {
                return Err(invalid(&key, "an exec command needs `command`"));
            }
            QuickCommandKind::Alias if resolves_to_itself(config, name) => {
                return Err(invalid(&key, "the alias resolves to itself"));
            }
            _ => {}
        }
    }
    Ok(())
}

/// The command token of an alias target (`/deploy prod` -> `deploy`).
#[must_use]
pub fn target_name(entry: &QuickCommand) -> Option<String> {
    let target = entry.target.as_deref()?;
    let token = target.trim().trim_start_matches('/');
    let name = token.split_whitespace().next()?;
    Some(name.to_lowercase())
}

/// Whether following alias targets from `start` comes back to it.
fn resolves_to_itself(config: &Config, start: &str) -> bool {
    let mut current = start.to_owned();
    // Each step visits a distinct entry, so the walk ends within the
    // table's length unless it loops.
    for _ in 0..=config.quick_commands.len() {
        let Some(entry) = config.quick_commands.get(&current) else {
            return false;
        };
        if entry.kind != QuickCommandKind::Alias {
            return false;
        }
        let Some(next) = target_name(entry) else {
            return false;
        };
        if next == start {
            return true;
        }
        current = next;
    }
    false
}

fn invalid(key: &str, what: &str) -> Error {
    Error::new(&codes::CONFIG_INVALID, format!("{key}: {what}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alias(target: &str) -> QuickCommand {
        QuickCommand {
            kind: QuickCommandKind::Alias,
            target: Some(target.to_owned()),
            command: None,
            timeout_secs: 30,
        }
    }

    #[test]
    fn an_alias_cycle_is_refused_and_a_chain_is_not() {
        let mut config = Config::default();
        config.quick_commands.insert("a".into(), alias("/b now"));
        config.quick_commands.insert("b".into(), alias("model"));
        assert!(check(&config).is_ok());
        config.quick_commands.insert("b".into(), alias("/A"));
        let err = check(&config).unwrap_err();
        assert_eq!(err.code(), &codes::CONFIG_INVALID);
        assert!(err.message().ends_with("the alias resolves to itself"));
        config.quick_commands.clear();
        config
            .quick_commands
            .insert("self".into(), alias("/self x"));
        assert!(check(&config).is_err());
    }

    #[test]
    fn an_exec_without_a_command_is_refused() {
        let mut config = Config::default();
        config.quick_commands.insert(
            "slow".into(),
            QuickCommand {
                kind: QuickCommandKind::Exec,
                target: None,
                command: None,
                timeout_secs: 30,
            },
        );
        let err = check(&config).unwrap_err();
        assert_eq!(
            err.message(),
            "quick_commands.slow: an exec command needs `command`"
        );
    }
}
