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
    if !valid_branch_prefix(&delegation.worktree_branch_prefix) {
        return Err(Error::new(
            &codes::CONFIG_INVALID,
            format!(
                "delegation.worktree_branch_prefix: '{}' is not a branch prefix (1-64 characters \
                 of [A-Za-z0-9._/-], no '..', no leading or trailing '/')",
                delegation.worktree_branch_prefix
            ),
        ));
    }
    Ok(())
}

/// Whether `prefix` makes valid branch names `<prefix>/subagent-<id>`.
fn valid_branch_prefix(prefix: &str) -> bool {
    (1..=64).contains(&prefix.len())
        && prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
        && !prefix.contains("..")
        && !prefix.contains("//")
        && !prefix.starts_with(['/', '.', '-'])
        && !prefix.ends_with('/')
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

    #[test]
    fn a_branch_prefix_must_make_valid_branch_names() {
        let mut config = Config::default();
        for good in ["prometheus", "agents/work", "x.y_z-1"] {
            config.delegation.worktree_branch_prefix = good.to_owned();
            check(&config).unwrap();
        }
        for bad in ["", "a b", "../x", "x/", "/x", "a//b"] {
            config.delegation.worktree_branch_prefix = bad.to_owned();
            let err = check(&config).unwrap_err();
            assert!(
                err.message()
                    .starts_with("delegation.worktree_branch_prefix"),
                "{bad}"
            );
        }
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
        assert!(!d.worktree_isolation);
        assert_eq!(d.worktree_branch_prefix, "prometheus");
        let w = Config::default().worktrees;
        assert!(w.sync_remote && w.archive_dirty);
        assert_eq!(
            (
                w.archive_max_file_bytes,
                w.max_total_bytes,
                w.archive_retention_secs
            ),
            (52_428_800, 0, 2_592_000)
        );
    }
}
