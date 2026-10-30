//! Applies the Landlock ruleset and seccomp filters in the child, after
//! `fork` and before `exec`. The only `unsafe` on Linux (health.toml
//! allowlist): `pre_exec` closures run in a forked copy of a possibly
//! multi-threaded process, so everything they do must be async-signal-safe.
//! Both the ruleset and the BPF programs are built in the parent; the
//! closure only issues `prctl`, `landlock_restrict_self` and `seccomp`
//! system calls and reports failures through errno, never allocating.

#![allow(unsafe_code)]

use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;

use landlock::{RulesetCreated, RulesetStatus};

use super::seccomp::Program;

/// Makes `command` confine its child with `ruleset` and `filters`.
pub(super) fn confine(
    command: &mut Command,
    ruleset: Option<RulesetCreated>,
    filters: Vec<Program>,
) {
    let mut ruleset = ruleset;
    let hook = move || -> io::Result<()> {
        if let Some(ruleset) = ruleset.take() {
            let status = ruleset
                .restrict_self()
                .map_err(|_| io::Error::from_raw_os_error(libc::EPERM))?;
            if status.ruleset != RulesetStatus::FullyEnforced {
                return Err(io::Error::from_raw_os_error(libc::EPERM));
            }
        }
        for filter in &filters {
            seccompiler::apply_filter(filter)
                .map_err(|_| io::Error::from_raw_os_error(libc::EPERM))?;
        }
        Ok(())
    };
    // SAFETY: the hook runs between fork and exec. It takes the ruleset out
    // of its own state and issues only prctl(2), landlock_restrict_self(2)
    // and seccomp(2) on data built before the fork; errors are raw errno
    // values (no allocation, no locks), so it is async-signal-safe.
    unsafe {
        command.pre_exec(hook);
    }
}
