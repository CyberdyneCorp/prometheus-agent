//! Checks of `[security.egress]` credential injection (spec:
//! security-boundaries, Credential injection at the egress proxy): each
//! `security.egress.inject` key names a secret and maps it to at least one
//! host pattern.

use pmt_core::error::{Error, codes};

use crate::schema::{Config, is_secret_name};

/// Fails with `config.invalid` naming the first bad mapping.
///
/// # Errors
///
/// `config.invalid` (exit 78).
pub fn check(config: &Config) -> Result<(), Error> {
    for (name, hosts) in &config.security.egress.inject {
        if !is_secret_name(name) {
            return Err(Error::new(
                &codes::CONFIG_INVALID,
                format!(
                    "security.egress.inject.{name}: not a secret name (^[A-Z][A-Z0-9_]{{0,127}}$)"
                ),
            ));
        }
        if hosts.iter().all(|h| h.trim().is_empty()) {
            return Err(Error::new(
                &codes::CONFIG_INVALID,
                format!("security.egress.inject.{name}: map it to at least one host pattern"),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn mappings_need_a_secret_name_and_a_host() {
        assert!(
            check(&config(
                "[security.egress.inject]\nGITHUB_TOKEN = [\"api.github.com\"]\n"
            ))
            .is_ok()
        );
        let err = check(&config("[security.egress.inject]\ngithub = [\"x\"]\n")).unwrap_err();
        assert!(
            err.message().contains("not a secret name"),
            "{}",
            err.message()
        );
        let err = check(&config("[security.egress.inject]\nGITHUB_TOKEN = []\n")).unwrap_err();
        assert!(
            err.message().contains("at least one host"),
            "{}",
            err.message()
        );
    }
}
