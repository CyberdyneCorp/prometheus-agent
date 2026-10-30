//! `perf-sampler BINARY RUNS`: runs `BINARY --version` and prints one
//! `startup_ms <ms>` line per timed run, then `peak_rss_bytes <n|none>`.
//!
//! This is a separate, std-and-nix-only binary rather than an xtask
//! subcommand because peak RSS is read with `getrusage(RUSAGE_CHILDREN)`,
//! and on Linux a child inherits a floor from the process that spawned it:
//! std spawns through `posix_spawn`, which glibc implements as
//! `clone(CLONE_VM | CLONE_VFORK)`, and at `execve` the kernel folds the
//! shared (parent's) memory high-water mark into the child's `maxrss`. A
//! small spawner keeps that floor small, and [`attributable`] discards a
//! peak that does not rise above it. It also keeps `cargo build`, which the
//! `perf` parent runs, out of the children's counter.

use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Instant;

/// Runs excluded from the samples: the first launch of a fresh binary pays
/// for page-cache misses and, on macOS, the code-signature check.
const WARMUP_RUNS: usize = 3;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [binary, runs] = args.as_slice() else {
        eprintln!("usage: perf-sampler BINARY RUNS");
        return ExitCode::FAILURE;
    };
    let Ok(runs) = runs.parse::<usize>() else {
        eprintln!("perf-sampler: RUNS must be a number, got {runs}");
        return ExitCode::FAILURE;
    };
    match sample(Path::new(binary), runs) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("perf-sampler: {message}");
            ExitCode::FAILURE
        }
    }
}

fn sample(binary: &Path, runs: usize) -> Result<(), String> {
    for _ in 0..WARMUP_RUNS {
        run_once(binary)?;
    }
    for _ in 0..runs {
        println!("startup_ms {}", run_once(binary)?);
    }
    // Read after the runs: the floor only grows, so the final value bounds
    // the floor every run inherited.
    match attributable(children_peak_rss(), inherited_floor()) {
        Some(bytes) => println!("peak_rss_bytes {bytes}"),
        None => println!("peak_rss_bytes none"),
    }
    Ok(())
}

fn run_once(binary: &Path) -> Result<f64, String> {
    let start = Instant::now();
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|e| format!("{}: {e}", binary.display()))?;
    let elapsed = start.elapsed();
    if !output.status.success() || !output.stdout.starts_with(b"prometheus ") {
        return Err(format!(
            "{} --version failed ({}): {}",
            binary.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(elapsed.as_secs_f64() * 1_000.0)
}

/// The children's peak, unless it does not exceed the floor every child
/// inherited from this process: then it is this process's footprint, not
/// the measured binary's, and is reported as not measurable.
fn attributable(children_peak: Option<u64>, floor: Option<u64>) -> Option<u64> {
    let peak = children_peak?;
    match floor {
        Some(floor) if peak <= floor => None,
        _ => Some(peak),
    }
}

#[cfg(unix)]
fn children_peak_rss() -> Option<u64> {
    use nix::sys::resource::{UsageWho, getrusage};
    let raw = u64::try_from(getrusage(UsageWho::RUSAGE_CHILDREN).ok()?.max_rss()).ok()?;
    // Darwin reports ru_maxrss in bytes, Linux and the BSDs in KiB.
    Some(if cfg!(target_vendor = "apple") {
        raw
    } else {
        raw * 1024
    })
}

#[cfg(not(unix))]
fn children_peak_rss() -> Option<u64> {
    None
}

/// This process's memory high-water mark (`VmHWM`), which is what a
/// vfork-spawned child starts its `maxrss` from on Linux.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn inherited_floor() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

/// Elsewhere a spawned child starts with a fresh address space and counter.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn inherited_floor() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::attributable;

    #[test]
    fn a_peak_at_or_below_the_inherited_floor_is_not_measurable() {
        assert_eq!(attributable(Some(5_000), Some(5_000)), None);
        assert_eq!(attributable(Some(4_000), Some(5_000)), None);
    }

    #[test]
    fn a_peak_above_the_floor_belongs_to_the_measured_binary() {
        assert_eq!(attributable(Some(6_000), Some(5_000)), Some(6_000));
    }

    #[test]
    fn without_a_floor_the_reported_peak_is_used_as_is() {
        assert_eq!(attributable(Some(6_000), None), Some(6_000));
        assert_eq!(attributable(None, None), None);
    }
}
