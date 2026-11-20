//! The opt-in gate (spec: harness-evolution, Opt-in per profile).

use pmt_config::Config;
use pmt_core::error::{Error, codes};

/// Refuses evolution on a profile without `evolve.enabled = true`: every
/// `prometheus evolve` subcommand, `/evolve`, the `evolve.*` methods and
/// scheduled or automatic jobs call this before touching anything.
///
/// # Errors
///
/// `evolve.disabled` (exit 78).
pub fn require_enabled(config: &Config, profile: &str) -> Result<(), Error> {
    if config.evolve.enabled {
        return Ok(());
    }
    Err(Error::new(
        &codes::EVOLVE_DISABLED,
        format!("harness evolution is disabled for profile '{profile}'; set evolve.enabled = true"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_without_the_section_is_refused_with_exit_78() {
        let err = require_enabled(&Config::default(), "work").unwrap_err();
        assert_eq!(err.exit_code(), 78);
        assert_eq!(
            err.message(),
            "harness evolution is disabled for profile 'work'; set evolve.enabled = true"
        );
        let mut on = Config::default();
        on.evolve.enabled = true;
        assert!(require_enabled(&on, "work").is_ok());
    }
}
