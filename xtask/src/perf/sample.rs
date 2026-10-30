//! Runs the measured binary repeatedly and records wall time and peak RSS.
//!
//! This runs in a dedicated child process (`cargo xtask perf-sample`) because
//! peak RSS comes from `getrusage(RUSAGE_CHILDREN)`, which covers every child
//! ever reaped: in the parent it would include `cargo build`.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Runs excluded from the samples: the first launch of a fresh binary pays
/// for page-cache misses and, on macOS, the code-signature check.
const WARMUP_RUNS: usize = 3;

/// What one `perf-sample` process measured, passed back to the parent as JSON.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Samples {
    /// Wall time of each `--version` run, in milliseconds.
    pub(crate) startup_ms: Vec<f64>,
    /// Largest resident set of any run, when the platform reports it.
    pub(crate) peak_rss_bytes: Option<u64>,
}

/// Runs `binary --version` `runs` times after the warm-up runs.
pub(crate) fn collect(binary: &Path, runs: usize) -> Result<Samples, String> {
    for _ in 0..WARMUP_RUNS {
        run_once(binary)?;
    }
    let startup_ms = (0..runs)
        .map(|_| run_once(binary))
        .collect::<Result<_, _>>()?;
    Ok(Samples {
        startup_ms,
        peak_rss_bytes: children_peak_rss(),
    })
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
