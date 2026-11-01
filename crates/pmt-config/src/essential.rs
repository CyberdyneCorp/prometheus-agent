//! The essential skill cannot be disabled (spec: skills, Disabled skills).
//!
//! `prometheus-agent` teaches the agent how to operate Prometheus itself
//! (configuration, profiles, troubleshooting); a profile without it cannot
//! be talked out of a broken setup, so a config that disables it, globally
//! or for one surface, does not load.

use pmt_core::error::{Error, codes};

use crate::schema::Config;

/// The skill that is always enabled.
pub const ESSENTIAL_SKILL: &str = "prometheus-agent";

/// Refuses a config that disables [`ESSENTIAL_SKILL`].
///
/// # Errors
///
/// `config.invalid` naming the key and the skill.
pub fn check(config: &Config) -> Result<(), Error> {
    let skills = &config.skills;
    let mut keys: Vec<String> = Vec::new();
    if skills.disabled.iter().any(|n| n == ESSENTIAL_SKILL) {
        keys.push("skills.disabled".to_owned());
    }
    for (platform, names) in &skills.platform_disabled {
        if names.iter().any(|n| n == ESSENTIAL_SKILL) {
            keys.push(format!("skills.platform_disabled.{platform}"));
        }
    }
    match keys.first() {
        None => Ok(()),
        Some(key) => Err(Error::new(
            &codes::CONFIG_INVALID,
            format!("{key}: the essential skill '{ESSENTIAL_SKILL}' cannot be disabled"),
        )
        .with_hint(format!(
            "remove '{ESSENTIAL_SKILL}' from {}",
            keys.join(", ")
        ))),
    }
}

/// Every whole-config check beyond the schema: provider transports,
/// quick commands and the essential skill. Run when a profile loads and before any write.
///
/// # Errors
///
/// The first failing check's error.
pub fn semantic(config: &Config) -> Result<(), Error> {
    crate::provider_urls::check(config)?;
    crate::quick_commands::check(config)?;
    check(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabling_the_essential_skill_is_refused() {
        let mut config = Config::default();
        assert!(check(&config).is_ok());
        config.skills.disabled = vec!["x".into(), ESSENTIAL_SKILL.into()];
        let err = check(&config).unwrap_err();
        assert_eq!(err.code(), &codes::CONFIG_INVALID);
        assert!(
            err.message().contains("skills.disabled"),
            "{}",
            err.message()
        );
        assert!(err.message().contains(ESSENTIAL_SKILL));
        config.skills.disabled.clear();
        config
            .skills
            .platform_disabled
            .insert("telegram".into(), vec![ESSENTIAL_SKILL.into()]);
        let err = check(&config).unwrap_err();
        assert!(
            err.message()
                .starts_with("skills.platform_disabled.telegram:")
        );
    }
}
