//! Checks of group gating and role grants beyond their schema (spec:
//! messaging-gateway, Group mention gating and observe mode; Role-based
//! access grants): at most 32 mention patterns, each a valid regular
//! expression, and role grants given as ids, not names.

use pmt_core::error::{Error, codes};

use crate::Config;

/// Mention patterns one table may list.
const MAX_PATTERNS: usize = 32;

/// Refuses invalid mention patterns and role names in `allow_roles`.
///
/// # Errors
///
/// `config.invalid` naming the key.
pub(crate) fn check(config: &Config) -> Result<(), Error> {
    let groups = &config.gateway.groups;
    let tables = groups
        .mention_patterns
        .iter()
        .map(|p| ("gateway.groups.mention_patterns".to_owned(), p))
        .chain(groups.platforms.iter().filter_map(|(name, table)| {
            table.mention_patterns.as_ref().map(|p| {
                (
                    format!("gateway.groups.platforms.{name}.mention_patterns"),
                    p,
                )
            })
        }));
    for (key, patterns) in tables {
        check_patterns(&key, patterns)?;
    }
    for (platform, table) in &config.gateway.platforms {
        for role in &table.allow_roles {
            if !role_id_shaped(platform, role) {
                return Err(Error::new(
                    &codes::CONFIG_INVALID,
                    format!(
                        "gateway.platforms.{platform}.allow_roles: '{role}' is not a role id; {}",
                        id_hint(platform)
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn check_patterns(key: &str, patterns: &[String]) -> Result<(), Error> {
    if patterns.len() > MAX_PATTERNS {
        return Err(Error::new(
            &codes::CONFIG_INVALID,
            format!("{key}: {} patterns; at most {MAX_PATTERNS}", patterns.len()),
        ));
    }
    for pattern in patterns {
        if let Err(err) = regex::RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
        {
            return Err(Error::new(
                &codes::CONFIG_INVALID,
                format!("{key}: '{pattern}' is not a valid regular expression: {err}"),
            ));
        }
    }
    Ok(())
}

/// Whether `role` looks like the platform's role id (a name does not).
fn role_id_shaped(platform: &str, role: &str) -> bool {
    match platform {
        "discord" => !role.is_empty() && role.chars().all(|c| c.is_ascii_digit()),
        "slack" => {
            role.starts_with('S')
                && role.len() > 1
                && role.chars().all(|c| c.is_ascii_alphanumeric())
        }
        "matrix" => role.parse::<i64>().is_ok(),
        _ => !role.trim().is_empty() && !role.contains(char::is_whitespace),
    }
}

fn id_hint(platform: &str) -> &'static str {
    match platform {
        "discord" => {
            "Discord role ids are numbers (Server Settings > Roles > Copy Role ID with developer mode on)"
        }
        "slack" => "Slack user group ids start with S (from usergroups.list)",
        "matrix" => "Matrix roles are power levels (numbers such as 50 or 100)",
        _ => "give the platform's role id",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml: &str) -> Config {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn a_role_name_and_a_bad_pattern_are_refused() {
        let name = config("[gateway.platforms.discord]\nallow_roles = [\"moderators\"]\n");
        let err = check(&name).unwrap_err();
        assert!(
            err.message().contains("'moderators' is not a role id"),
            "{err}"
        );
        check(&config(
            "[gateway.platforms.discord]\nallow_roles = [\"111222333\"]\n",
        ))
        .unwrap();
        let bad = config("[gateway.groups.platforms.slack]\nmention_patterns = [\"(unclosed\"]\n");
        let err = check(&bad).unwrap_err();
        assert!(
            err.message()
                .starts_with("gateway.groups.platforms.slack.mention_patterns")
        );
        let many = format!(
            "[gateway.groups]\nmention_patterns = [{}]\n",
            vec!["\"hey\""; 33].join(", ")
        );
        assert!(check(&config(&many)).is_err());
    }
}
