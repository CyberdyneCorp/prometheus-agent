//! Agent tools never change budgets (spec: observability, Budget warnings
//! and overrides, "Model cannot raise its own budget"): a config-setting
//! tool asks [`check_key`], file tools ([`ConfigWriteGuard`]) ask before
//! writing any file. Users change budgets with `prometheus config set` or
//! `/budget set`, which do not pass through these guards.

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};

/// Refuses a config key an agent tool wants to set when it is a budget.
///
/// # Errors
///
/// `budget.agent_write` (permission denied, exit 77).
pub fn check_key(key: &str) -> Result<(), Error> {
    let key = key.trim();
    if key == "budgets" || key.starts_with("budgets.") {
        return Err(refused(&format!("'{key}'")));
    }
    Ok(())
}

fn refused(what: &str) -> Error {
    Error::new(
        &codes::BUDGET_AGENT_WRITE,
        format!(
            "agent tools may not change budgets ({what}); budgets are changed only by the user"
        ),
    )
    .with_hint("ask the user to run: prometheus config set budgets.<key> <value> (or /budget set)")
}

/// Guards agent file writes against changing `[budgets]` of any profile's
/// `config.toml` under one Prometheus home.
#[derive(Debug, Clone)]
pub struct ConfigWriteGuard {
    profiles: PathBuf,
}

impl ConfigWriteGuard {
    /// A guard for the profiles under `home`.
    #[must_use]
    pub fn new(home: &Path) -> Self {
        let profiles = home.join("profiles");
        Self {
            profiles: profiles.canonicalize().unwrap_or(profiles),
        }
    }

    /// Checks that writing `new_text` to `target` leaves every budget as
    /// it is. Writes to other files pass. A new text that does not parse
    /// as TOML is refused for a profile config (its budgets cannot be
    /// compared).
    ///
    /// # Errors
    ///
    /// `budget.agent_write` (permission denied, exit 77).
    pub fn check(&self, target: &Path, new_text: &str) -> Result<(), Error> {
        let Some(config) = self.profile_config(target) else {
            return Ok(());
        };
        let current = std::fs::read_to_string(&config).unwrap_or_default();
        let before = budgets(&current).unwrap_or(None);
        match budgets(new_text) {
            Ok(after) if after == before => Ok(()),
            _ => Err(refused(&format!("[budgets] in {}", config.display()))),
        }
    }

    /// `target`, resolved through symlinks, when it is
    /// `<home>/profiles/<name>/config.toml`.
    fn profile_config(&self, target: &Path) -> Option<PathBuf> {
        let resolved = resolve(target)?;
        let name = resolved.file_name()?.to_str()?;
        let profile = resolved.parent()?;
        (name.eq_ignore_ascii_case("config.toml") && profile.parent()? == self.profiles)
            .then_some(resolved)
    }
}

/// The canonical path of `target`, or of its parent plus its name when
/// the file does not exist yet.
fn resolve(target: &Path) -> Option<PathBuf> {
    if let Ok(path) = target.canonicalize() {
        return Some(path);
    }
    let parent = target.parent().filter(|p| !p.as_os_str().is_empty())?;
    Some(parent.canonicalize().ok()?.join(target.file_name()?))
}

/// The `[budgets]` table of a TOML document (`None` without one).
fn budgets(text: &str) -> Result<Option<toml::Value>, toml::de::Error> {
    let table: toml::Table = text.parse()?;
    Ok(table.get("budgets").cloned())
}

#[cfg(test)]
mod tests {
    use super::{ConfigWriteGuard, check_key};

    #[test]
    fn budget_keys_are_refused_with_permission_denied() {
        for key in ["budgets.root_usd", "budgets", " budgets.allow_unpriced"] {
            let err = check_key(key).unwrap_err();
            assert_eq!(err.code().code, "budget.agent_write");
            assert_eq!(err.exit_code(), 77);
        }
        assert!(check_key("display.theme").is_ok());
        assert!(check_key("budgetsx").is_ok());
    }

    #[test]
    fn file_writes_may_not_change_profile_budgets() {
        let home = tempfile::tempdir().unwrap();
        let profile = home.path().join("profiles/work");
        std::fs::create_dir_all(&profile).unwrap();
        let config = profile.join("config.toml");
        std::fs::write(&config, "[budgets]\nroot_usd = 1.0\n").unwrap();
        let guard = ConfigWriteGuard::new(home.path());

        let raised = guard
            .check(&config, "[budgets]\nroot_usd = 100.0\n")
            .unwrap_err();
        assert_eq!(raised.exit_code(), 77);
        assert!(guard.check(&config, "").is_err(), "removing budgets");
        assert!(guard.check(&config, "[budgets\n").is_err(), "unparsable");
        assert!(
            guard
                .check(
                    &config,
                    "[budgets]\nroot_usd = 1.0\n[display]\ntheme = \"mono\"\n"
                )
                .is_ok(),
            "other keys may change"
        );
        assert!(
            guard
                .check(&home.path().join("notes.toml"), "[budgets]\nx = 1\n")
                .is_ok()
        );

        // A relative path through `..` and a symlink resolve to the config.
        let indirect = profile.join("../work/config.toml");
        assert!(
            guard
                .check(&indirect, "[budgets]\nroot_usd = 9.0\n")
                .is_err()
        );
        #[cfg(unix)]
        {
            let link = home.path().join("innocent.toml");
            std::os::unix::fs::symlink(&config, &link).unwrap();
            assert!(guard.check(&link, "[budgets]\nroot_usd = 9.0\n").is_err());
        }
        // A profile without a config yet: adding budgets is refused.
        let fresh = home.path().join("profiles/new");
        std::fs::create_dir_all(&fresh).unwrap();
        assert!(
            guard
                .check(&fresh.join("config.toml"), "[budgets]\nroot_usd = 9.0\n")
                .is_err()
        );
        assert!(
            guard
                .check(&fresh.join("config.toml"), "[display]\n")
                .is_ok()
        );
    }
}
