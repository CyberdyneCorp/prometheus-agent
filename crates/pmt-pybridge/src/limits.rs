//! Resource limits of a sidecar's process tree (spec: python-bridge,
//! Resource limits): resident memory and process count, sampled twice a
//! second across the sidecar and every descendant. Exceeding a limit
//! terminates the tree as a crash whose status names the limit.

use std::collections::HashSet;
use std::time::Duration;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// How often the tree is sampled.
pub const SAMPLE_EVERY: Duration = Duration::from_millis(500);

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

/// Samples the tree rooted at `pid` (`None` when the root is gone).
#[must_use]
pub fn sample(system: &mut System, pid: u32) -> Option<Usage> {
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
    let memory = tree
        .iter()
        .filter_map(|p| system.process(*p))
        .map(sysinfo::Process::memory)
        .sum();
    Some(Usage {
        memory,
        processes: u32::try_from(tree.len()).unwrap_or(u32::MAX),
    })
}

impl Limits {
    /// The limit `usage` exceeds, as the crash status names it.
    #[must_use]
    pub fn exceeded(&self, usage: Usage) -> Option<String> {
        if usage.memory > self.max_memory_bytes {
            return Some(format!(
                "resource limit exceeded (max_memory_bytes {} MiB)",
                self.max_memory_bytes / (1024 * 1024)
            ));
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
        let usage = sample(&mut system, std::process::id()).unwrap();
        assert!(usage.memory > 0 && usage.processes >= 1);
    }
}
