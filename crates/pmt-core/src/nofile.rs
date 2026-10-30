//! Raising the open-file soft limit at startup
//! (spec: workspace-architecture, File-descriptor soft limit).
//!
//! macOS shells default to a soft limit of 256, which a daemon with many
//! adapters, MCP servers and sockets exhausts quickly. Every process raises
//! its soft limit towards `process.nofile_soft_limit` (`daemon.toml`), never
//! above the hard limit and never lowering a limit that is already higher.
//! The soft limit is per process and inherited across `fork`/`exec`, so
//! every child (tools, MCP servers, runtimes) starts with the raised value.

/// The soft limit before and after the raise, and the configured target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NofileLimit {
    /// Soft limit at startup.
    pub before: u64,
    /// Soft limit now in force.
    pub after: u64,
    /// `process.nofile_soft_limit`.
    pub target: u64,
}

impl NofileLimit {
    /// Whether the limit in force is below the configured target, which
    /// the caller logs at WARN.
    #[must_use]
    pub fn below_target(&self) -> bool {
        self.after < self.target
    }

    /// The WARN line for a limit below its target.
    #[must_use]
    pub fn warning(&self) -> String {
        format!(
            "open-file soft limit is {}, below process.nofile_soft_limit = {}; raise the hard limit (ulimit -Hn) or lower the setting in daemon.toml",
            self.after, self.target
        )
    }
}

/// The soft limit to set, or `None` to leave it alone: a target of 0
/// disables the raise, a soft limit already at or above the target is
/// kept, and the hard limit caps the result.
#[must_use]
pub fn planned_soft_limit(soft: u64, hard: u64, target: u64) -> Option<u64> {
    let wanted = target.min(hard);
    (target != 0 && wanted > soft).then_some(wanted)
}

/// Raises this process's soft limit towards `target`. Returns `None` when
/// `target` is 0 or the host is not POSIX (Windows has no such limit).
/// When the kernel refuses the new value the limit stays as it was and the
/// result reports it as below target.
///
/// # Errors
///
/// Reading the current limit failed.
pub fn raise(target: u64) -> std::io::Result<Option<NofileLimit>> {
    if target == 0 {
        return Ok(None);
    }
    imp::raise(target)
}

#[cfg(unix)]
mod imp {
    use nix::sys::resource::{Resource, getrlimit, setrlimit};

    use super::{NofileLimit, planned_soft_limit};

    pub(super) fn raise(target: u64) -> std::io::Result<Option<NofileLimit>> {
        let (soft, hard) = getrlimit(Resource::RLIMIT_NOFILE)?;
        let after = match planned_soft_limit(soft, hard, target) {
            // A refusal (macOS caps the soft limit below an infinite hard
            // limit) keeps the old value; the caller warns about it.
            Some(new) if setrlimit(Resource::RLIMIT_NOFILE, new, hard).is_ok() => new,
            _ => soft,
        };
        Ok(Some(NofileLimit {
            before: soft,
            after,
            target,
        }))
    }
}

#[cfg(not(unix))]
mod imp {
    use super::NofileLimit;

    #[allow(clippy::unnecessary_wraps)]
    pub(super) fn raise(_target: u64) -> std::io::Result<Option<NofileLimit>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::{NofileLimit, planned_soft_limit};

    #[test]
    fn low_limit_is_raised_to_the_target() {
        assert_eq!(planned_soft_limit(256, 65_536, 4096), Some(4096));
    }

    #[test]
    fn higher_limit_is_never_lowered() {
        assert_eq!(planned_soft_limit(10_240, 65_536, 4096), None);
        assert_eq!(planned_soft_limit(4096, 65_536, 4096), None);
    }

    #[test]
    fn hard_limit_caps_the_raise() {
        assert_eq!(planned_soft_limit(256, 1024, 4096), Some(1024));
        assert_eq!(planned_soft_limit(1024, 1024, 4096), None);
    }

    #[test]
    fn zero_disables_the_raise() {
        assert_eq!(planned_soft_limit(256, 65_536, 0), None);
        assert_eq!(super::raise(0).unwrap(), None);
    }

    #[test]
    fn below_target_is_reported() {
        let capped = NofileLimit {
            before: 256,
            after: 1024,
            target: 4096,
        };
        assert!(capped.below_target());
        assert!(
            capped
                .warning()
                .contains("process.nofile_soft_limit = 4096")
        );
        let met = NofileLimit {
            after: 4096,
            ..capped
        };
        assert!(!met.below_target());
    }
}
