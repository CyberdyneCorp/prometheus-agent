//! The migration components (spec: hermes-migration, Migration command
//! surface): `--only` and `--skip` select among them, and each is staged
//! and committed on its own.

use std::collections::BTreeSet;
use std::fmt;

use pmt_core::error::{Error, codes};
use serde::Serialize;

/// One part of a Hermes profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Component {
    /// `config.yaml` keys.
    Config,
    /// `.env` and credential files.
    Secrets,
    /// `SOUL.md` and the personality selection.
    Identity,
    /// `memories/MEMORY.md` and `memories/USER.md`.
    Memory,
    /// `skills/`.
    Skills,
    /// `state.db` sessions.
    Sessions,
    /// `cron/jobs.json` and its scripts.
    Cron,
    /// `mcp_servers`.
    Mcp,
    /// `plugins/` and built-in integrations.
    Plugins,
    /// Messaging platforms and pairings.
    Gateway,
}

impl Component {
    /// Every component, in the order the spec lists them.
    pub const ALL: [Self; 10] = [
        Self::Config,
        Self::Secrets,
        Self::Identity,
        Self::Memory,
        Self::Skills,
        Self::Sessions,
        Self::Cron,
        Self::Mcp,
        Self::Plugins,
        Self::Gateway,
    ];

    /// The order components commit in: secrets before anything that
    /// references a secret, config before the components that add keys.
    pub const COMMIT_ORDER: [Self; 10] = [
        Self::Secrets,
        Self::Config,
        Self::Identity,
        Self::Memory,
        Self::Skills,
        Self::Sessions,
        Self::Cron,
        Self::Mcp,
        Self::Plugins,
        Self::Gateway,
    ];

    /// The command-line name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Secrets => "secrets",
            Self::Identity => "identity",
            Self::Memory => "memory",
            Self::Skills => "skills",
            Self::Sessions => "sessions",
            Self::Cron => "cron",
            Self::Mcp => "mcp",
            Self::Plugins => "plugins",
            Self::Gateway => "gateway",
        }
    }

    /// Parses one name.
    ///
    /// # Errors
    ///
    /// `migrate.unknown_component` (exit 2) listing the valid names.
    pub fn parse(name: &str) -> Result<Self, Error> {
        Self::ALL
            .into_iter()
            .find(|c| c.as_str() == name.trim())
            .ok_or_else(|| {
                let valid: Vec<&str> = Self::ALL.iter().map(|c| c.as_str()).collect();
                Error::new(
                    &codes::MIGRATE_UNKNOWN_COMPONENT,
                    format!(
                        "unknown component '{}'; valid components: {}",
                        name.trim(),
                        valid.join(", ")
                    ),
                )
            })
    }
}

impl fmt::Display for Component {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The selected components: `only` (comma-separated lists, empty = all)
/// minus `skip`.
///
/// # Errors
///
/// As [`Component::parse`].
pub fn select(only: &[String], skip: &[String]) -> Result<BTreeSet<Component>, Error> {
    let parse = |lists: &[String]| -> Result<BTreeSet<Component>, Error> {
        lists
            .iter()
            .flat_map(|l| l.split(','))
            .filter(|n| !n.trim().is_empty())
            .map(Component::parse)
            .collect()
    };
    let mut chosen = parse(only)?;
    if chosen.is_empty() {
        chosen = Component::ALL.into_iter().collect();
    }
    for c in parse(skip)? {
        chosen.remove(&c);
    }
    Ok(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_misspelled_component_lists_the_valid_names() {
        let err = select(&["sesions".to_owned()], &[]).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.message().contains("sesions"), "{}", err.message());
        assert!(
            err.message().contains(
                "config, secrets, identity, memory, skills, sessions, cron, mcp, plugins, gateway"
            ),
            "{}",
            err.message()
        );
    }

    #[test]
    fn only_and_skip_combine() {
        let chosen = select(&["config,sessions".to_owned()], &["sessions".to_owned()]).unwrap();
        assert_eq!(chosen.into_iter().collect::<Vec<_>>(), [Component::Config]);
        assert_eq!(select(&[], &[]).unwrap().len(), 10);
    }
}
