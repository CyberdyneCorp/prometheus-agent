//! Resource limits of a sidecar's process tree (spec: python-bridge,
//! Resource limits).
//!
//! The OS bounds what it can: on Linux the sandbox gives the sidecar a
//! data-segment rlimit of `max_memory_bytes` and makes it the subreaper
//! of its descendants, on Windows the sandbox's Job Object caps the whole
//! tree's committed memory and its process count. An allocation the OS
//! refuses ends the sidecar with [`MEMORY_EXIT`]. macOS accepts no usable
//! memory or per-tree process rlimit, so there the bound is this module's
//! sampler alone. On every OS the sampler also checks the tree as a whole
//! (resident memory summed over the sidecar and every descendant, and the
//! process count) twice a second, and remembers the tree's members so a
//! descendant that left the process group is still terminated with it.
//! Exceeding a limit terminates the tree as a crash whose status names it.

use std::collections::HashSet;
use std::time::Duration;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// How often the tree is sampled.
pub const SAMPLE_EVERY: Duration = Duration::from_millis(500);

/// The exit code of a sidecar whose allocation the OS refused (the shim
/// exits with it on `MemoryError`).
pub const MEMORY_EXIT: i32 = 86;

/// The bounds of one sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// `python_bridge.max_memory_bytes`.
    pub max_memory_bytes: u64,
    /// `python_bridge.max_processes`.
    pub max_processes: u32,
}

/// One sample of a tree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Resident bytes of every process.
    pub memory: u64,
    /// Processes in the tree.
    pub processes: u32,
}

/// A process of a tree: its pid and start time, so a recycled pid is
/// never mistaken for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Member {
    /// Process id.
    pub pid: u32,
    /// Start time (seconds since the epoch).
    pub start: u64,
}

/// One sample: the usage and every member but the root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tree {
    /// What the tree uses.
    pub usage: Usage,
    /// The descendants of the root.
    pub descendants: Vec<Member>,
}

/// Samples the tree rooted at `pid` (`None` when the root is gone).
#[must_use]
pub fn sample(system: &mut System, pid: u32) -> Option<Tree> {
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    let root = Pid::from_u32(pid);
    system.process(root)?;
    let mut tree: HashSet<Pid> = HashSet::from([root]);
    // Parents can be listed after their children: repeat until stable.
    loop {
        let before = tree.len();
        for (child, process) in system.processes() {
            if process.parent().is_some_and(|p| tree.contains(&p)) {
                tree.insert(*child);
            }
        }
        if tree.len() == before {
            break;
        }
    }
    let processes = || tree.iter().filter_map(|p| system.process(*p));
    let memory = processes().map(sysinfo::Process::memory).sum();
    let descendants = processes()
        .filter(|p| p.pid() != root)
        .map(|p| Member {
            pid: p.pid().as_u32(),
            start: p.start_time(),
        })
        .collect();
    Some(Tree {
        usage: Usage {
            memory,
            processes: u32::try_from(tree.len()).unwrap_or(u32::MAX),
        },
        descendants,
    })
}

/// Kills every member still running under the same start time.
pub fn kill_members(system: &mut System, members: &[Member]) {
    if members.is_empty() {
        return;
    }
    let pids: Vec<Pid> = members.iter().map(|m| Pid::from_u32(m.pid)).collect();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        true,
        ProcessRefreshKind::nothing(),
    );
    for member in members {
        if let Some(process) = system.process(Pid::from_u32(member.pid))
            && process.start_time() == member.start
        {
            process.kill();
        }
    }
}

impl Limits {
    /// The status of a tree over its memory limit.
    #[must_use]
    pub fn memory_exceeded(&self) -> String {
        format!(
            "resource limit exceeded (max_memory_bytes {} MiB)",
            self.max_memory_bytes / (1024 * 1024)
        )
    }

    /// The limit `usage` exceeds, as the crash status names it.
    #[must_use]
    pub fn exceeded(&self, usage: Usage) -> Option<String> {
        if usage.memory > self.max_memory_bytes {
            return Some(self.memory_exceeded());
        }
        (usage.processes > self.max_processes).then(|| {
            format!(
                "resource limit exceeded (max_processes {})",
                self.max_processes
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_names_the_limit() {
        let limits = Limits {
            max_memory_bytes: 1024 * 1024 * 1024,
            max_processes: 16,
        };
        let over = Usage {
            memory: 2 * 1024 * 1024 * 1024,
            processes: 1,
        };
        assert_eq!(
            limits.exceeded(over).as_deref(),
            Some("resource limit exceeded (max_memory_bytes 1024 MiB)")
        );
        assert!(
            limits
                .exceeded(Usage {
                    memory: 1,
                    processes: 17
                })
                .unwrap()
                .contains("max_processes")
        );
        assert!(
            limits
                .exceeded(Usage {
                    memory: 1,
                    processes: 1
                })
                .is_none()
        );
    }

    #[test]
    fn this_process_can_be_sampled() {
        let mut system = System::new();
        let tree = sample(&mut system, std::process::id()).unwrap();
        assert!(tree.usage.memory > 0 && tree.usage.processes >= 1);
    }
}
