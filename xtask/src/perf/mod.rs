//! `cargo xtask perf`: the startup, memory and size gate
//! (spec: workspace-architecture, Startup time budget; Memory and size
//! budgets).
//!
//! It builds the release binary, runs `prometheus --version` repeatedly in a
//! sampler child process, and fails when a measurement exceeds its absolute
//! budget or regresses more than 15% against the entry for this platform in
//! the baseline file. The chat and daemon budgets join when those commands
//! exist (M1).
//!
//! The gate is not part of `scripts/check`: wall-time samples on a busy
//! developer machine are too noisy for a blocking pre-push hook. CI runs it
//! in its own job on the reference runner (see `perf/README.md`).

mod budget;
mod sample;

use std::path::{Path, PathBuf};
use std::process::Command;

use self::budget::{Baseline, Metrics};

/// Measures, checks and optionally records the baseline entry for this
/// platform. A breached budget is never recorded.
pub(crate) fn run(
    root: &Path,
    runs: usize,
    baseline_path: &Path,
    update: bool,
) -> Result<(), String> {
    let binary = build_release(root)?;
    let current = measure(&binary, runs)?;
    let mut baseline = load_baseline(baseline_path)?;
    let platform = platform();
    report(&platform, &current, baseline.get(&platform));

    let stored = if update {
        None
    } else {
        baseline.get(&platform)
    };
    let violations = budget::evaluate(&current, stored);
    if !violations.is_empty() {
        for violation in &violations {
            eprintln!("perf: {violation}");
        }
        return Err(format!("{} performance violation(s)", violations.len()));
    }
    if update {
        baseline.insert(platform.clone(), current);
        save_baseline(baseline_path, &baseline)?;
        println!("perf: recorded {platform} in {}", baseline_path.display());
    }
    Ok(())
}

fn build_release(root: &Path) -> Result<PathBuf, String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(root)
        .args(["build", "--release", "--locked", "--package", "prometheus"])
        .status()
        .map_err(|e| format!("cargo build: {e}"))?;
    if !status.success() {
        return Err(format!("cargo build --release failed ({status})"));
    }
    let name = format!("prometheus{}", std::env::consts::EXE_SUFFIX);
    Ok(root.join("target/release").join(name))
}

/// Samples in a fresh `cargo xtask perf-sample` process so its peak-RSS
/// counter only sees the measured runs.
fn measure(binary: &Path, runs: usize) -> Result<Metrics, String> {
    let xtask = std::env::current_exe().map_err(|e| format!("current exe: {e}"))?;
    let output = Command::new(xtask)
        .arg("perf-sample")
        .arg("--runs")
        .arg(runs.to_string())
        .arg(binary)
        .output()
        .map_err(|e| format!("perf-sample: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "perf-sample failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let samples: sample::Samples =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("perf-sample output: {e}"))?;
    let (startup_median_ms, startup_p95_ms) = budget::summarize(samples.startup_ms);
    let binary_bytes = std::fs::metadata(binary)
        .map_err(|e| format!("{}: {e}", binary.display()))?
        .len();
    Ok(Metrics {
        startup_median_ms: round2(startup_median_ms),
        startup_p95_ms: round2(startup_p95_ms),
        peak_rss_bytes: samples.peak_rss_bytes,
        binary_bytes,
    })
}

/// Prints the sampler's JSON for the parent `perf` process.
pub(crate) fn sample_command(binary: &Path, runs: usize) -> Result<(), String> {
    let samples = sample::collect(binary, runs)?;
    let json = serde_json::to_string(&samples).map_err(|e| e.to_string())?;
    println!("{json}");
    Ok(())
}

fn report(platform: &str, current: &Metrics, stored: Option<&Metrics>) {
    let rss = current
        .peak_rss_bytes
        .map_or_else(|| "not reported".to_owned(), budget::mib);
    println!(
        "perf ({platform}): startup median {}, p95 {}; peak RSS {rss}; binary {}",
        budget::ms(current.startup_median_ms),
        budget::ms(current.startup_p95_ms),
        budget::mib(current.binary_bytes),
    );
    if stored.is_none() {
        println!("perf: no baseline for {platform}; only the absolute budgets apply");
    }
}

fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn load_baseline(path: &Path) -> Result<Baseline, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Baseline::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn save_baseline(path: &Path, baseline: &Baseline) -> Result<(), String> {
    let io_error = |e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io_error)?;
    }
    let json = serde_json::to_string_pretty(baseline).map_err(|e| e.to_string())?;
    std::fs::write(path, json + "\n").map_err(io_error)
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}
