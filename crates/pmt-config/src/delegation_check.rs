//! Whole-config checks of `[delegation]` the schema cannot express (spec:
//! subagent-delegation, Delegation configuration defaults): a stall
//! timeout is off (`0`) or at least 30 seconds, and the router is named.

use pmt_core::error::{Error, codes};

use crate::schema::Config;

/// The shortest stall timeout that is not `0`.
pub const MIN_INACTIVITY_SECS: u64 = 30;

/// Checks `[delegation]`.
///
/// # Errors
///
/// `config.invalid` naming the key and its accepted values.
pub fn check(config: &Config) -> Result<(), Error> {
    let delegation = &config.delegation;
    let timeout = delegation.inactivity_timeout_secs;
    if timeout != 0 && timeout < MIN_INACTIVITY_SECS {
        return Err(Error::new(
            &codes::CONFIG_INVALID,
            format!(
                "delegation.inactivity_timeout_secs: {timeout} is out of range (0 disables, \
                 otherwise at least {MIN_INACTIVITY_SECS})"
            ),
        ));
    }
    if delegation.router.trim().is_empty() {
        return Err(Error::new(
            &codes::CONFIG_INVALID,
            "delegation.router: the router name is empty (use \"default\")",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stall_timeout_is_off_or_at_least_thirty_seconds() {
        let mut config = Config::default();
        check(&config).unwrap();
        config.delegation.inactivity_timeout_secs = 10;
        let err = check(&config).unwrap_err();
        assert!(
            err.message()
                .starts_with("delegation.inactivity_timeout_secs")
        );
        config.delegation.inactivity_timeout_secs = 30;
        check(&config).unwrap();
    }

    /// The defaults the specification names.
    #[test]
    fn delegation_defaults_are_the_specified_ones() {
        let d = Config::default().delegation;
        assert_eq!(
            (
                d.max_concurrent_children,
                d.max_tasks_per_call,
                d.max_spawn_depth,
                d.max_iterations
            ),
            (8, 8, 1, 150)
        );
        assert_eq!(
            (
                d.inactivity_timeout_secs,
                d.max_summary_chars,
                d.oneshot_max_children
            ),
            (0, 24_000, 2,)
        );
        assert_eq!(d.child_approval, crate::schema::ChildApproval::Deny);
        assert_eq!(d.background, crate::schema::BackgroundMode::Auto);
        assert!(d.progress_events);
        assert_eq!((d.max_queued, d.escalation_timeout_secs), (32, 300));
    }
}
