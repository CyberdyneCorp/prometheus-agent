//! Tree names (spec: git-worktrees, Worktree creation; Sweep decision
//! table): user names match `[A-Za-z0-9._-]{1,64}`, unnamed trees get
//! `prom-<8 hex>`, subagent trees `subagent-<id>`.

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

/// Who named a tree, which sets its idle tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// `prom-<hex>`: an unnamed `chat -w` or `/worktree new`.
    Generated,
    /// A name the user gave.
    Named,
    /// `subagent-<id>`: a delegated child's isolation tree.
    Subagent,
    /// A kanban task's tree (never swept).
    Kanban,
}

impl Kind {
    /// The idle time after which the sweep may act on the tree.
    #[must_use]
    pub const fn tier_secs(self) -> u64 {
        match self {
            Self::Generated | Self::Subagent => 24 * 3600,
            Self::Named | Self::Kanban => 72 * 3600,
        }
    }

    /// The kind a name implies when nothing recorded it.
    #[must_use]
    pub fn of_name(name: &str) -> Self {
        if is_generated(name, "prom-") {
            Self::Generated
        } else if name.starts_with("subagent-") {
            Self::Subagent
        } else {
            Self::Named
        }
    }
}

fn is_generated(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .is_some_and(|hex| hex.len() == 8 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Checks a tree name.
///
/// # Errors
///
/// `worktree.invalid_name` when it is empty, longer than 64 characters or
/// has a character outside `[A-Za-z0-9._-]`.
pub fn validate(name: &str) -> Result<(), Error> {
    let valid = (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && name != "."
        && name != ".."
        && !name.to_ascii_lowercase().ends_with(".lock");
    if valid {
        Ok(())
    } else {
        Err(Error::new(
            &codes::WORKTREE_INVALID_NAME,
            format!("invalid worktree name '{name}': use 1-64 characters of [A-Za-z0-9._-]"),
        ))
    }
}

/// A fresh `prom-<8 hex>` name.
#[must_use]
pub fn generated() -> String {
    let mut bytes = [0u8; 4];
    if getrandom::fill(&mut bytes).is_err() {
        // The clock is unique enough for a local name when the OS has no
        // randomness to give.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        bytes = nanos.to_le_bytes();
    }
    let hex = u32::from_be_bytes(bytes);
    format!("prom-{hex:08x}")
}

/// The branch of tree `name` under `prefix` (`prometheus/<name>`).
#[must_use]
pub fn branch(prefix: &str, name: &str) -> String {
    format!("{}/{name}", prefix.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_names_are_prom_and_eight_hex_digits() {
        let name = generated();
        assert!(is_generated(&name, "prom-"), "{name}");
        assert_eq!(Kind::of_name(&name), Kind::Generated);
        validate(&name).unwrap();
    }

    #[test]
    fn names_outside_the_pattern_are_refused() {
        for bad in ["", "a b", "x/y", "..", "é", &"a".repeat(65), "x.lock"] {
            let err = validate(bad).unwrap_err();
            assert_eq!(err.code().code, "worktree.invalid_name", "{bad}");
        }
        for good in ["exp", "lane-x", "v1.2_rc", &"a".repeat(64)] {
            validate(good).unwrap();
        }
    }

    #[test]
    fn tiers_are_a_day_for_generated_and_three_days_for_named_trees() {
        assert_eq!(Kind::of_name("prom-0a1b2c3d").tier_secs(), 86_400);
        assert_eq!(Kind::of_name("subagent-sa-1").tier_secs(), 86_400);
        assert_eq!(Kind::of_name("lane-x").tier_secs(), 259_200);
    }
}
