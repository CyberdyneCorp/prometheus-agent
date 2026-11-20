//! The bot screen's free-memory floor (spec: execution-environments, Bot
//! screen: `bot_desktop.min_free_memory_bytes`).

use pmt_core::error::{Error, codes};

const MIB: u64 = 1024 * 1024;

/// Refuses a start with `free` bytes available under a floor of `min`
/// (`0`: no floor; an unknown `free` passes).
///
/// # Errors
///
/// `screen.unavailable` naming the threshold.
pub fn check(free: Option<u64>, min: u64) -> Result<(), Error> {
    match free {
        Some(free) if min > 0 && free < min => Err(Error::new(
            &codes::SCREEN_UNAVAILABLE,
            format!(
                "only {} MB of memory is free; the bot screen needs {} MB (bot_desktop.min_free_memory_bytes)",
                free / MIB,
                min / MIB
            ),
        )
        .with_hint("free memory, or lower bot_desktop.min_free_memory_bytes (0 turns the check off)")),
        _ => Ok(()),
    }
}

/// Free memory of this host, or of its cgroup when that is smaller.
#[must_use]
pub fn free() -> Option<u64> {
    let host = host_available();
    match (host, cgroup_available()) {
        (Some(host), Some(cgroup)) => Some(host.min(cgroup)),
        (host, cgroup) => host.or(cgroup),
    }
}

/// `MemAvailable` of `/proc/meminfo` (Linux); elsewhere what the system
/// reports as available.
fn host_available() -> Option<u64> {
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        return meminfo
            .lines()
            .find_map(|line| line.strip_prefix("MemAvailable:"))
            .and_then(|rest| {
                rest.trim()
                    .trim_end_matches("kB")
                    .trim()
                    .parse::<u64>()
                    .ok()
            })
            .map(|kib| kib * 1024);
    }
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    Some(system.available_memory())
}

/// What this process's cgroup (v2) may still use: `memory.max` minus
/// `memory.current`, when a limit is set.
fn cgroup_available() -> Option<u64> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = cgroup.lines().find_map(|line| line.strip_prefix("0::"))?;
    let dir = std::path::Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
    let max = read("memory.max")?.trim().parse::<u64>().ok()?;
    let current = read("memory.current")?.trim().parse::<u64>().ok()?;
    Some(max.saturating_sub(current))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Low memory.
    #[test]
    fn nine_hundred_mb_free_is_refused_naming_the_default_threshold() {
        let min = pmt_config::schema::BotDesktopConfig::default().min_free_memory_bytes;
        let refused = check(Some(900 * MIB), min).unwrap_err();
        assert_eq!(refused.code(), &codes::SCREEN_UNAVAILABLE);
        assert_eq!(refused.exit_code(), 69);
        assert!(
            refused.message().contains("1536 MB"),
            "{}",
            refused.message()
        );
        assert!(check(Some(900 * MIB), 0).is_ok(), "0 turns the floor off");
        assert!(check(Some(2048 * MIB), min).is_ok());
    }
}
