//! `cargo xtask perf`: the startup, memory and size gate
//! (spec: workspace-architecture, Startup time budget; Memory and size
//! budgets).
//!
//! It builds the release binary, runs `prometheus --version` repeatedly from
//! the small `perf-sampler` binary, and fails when a measurement exceeds its
//! absolute budget or regresses more than 15% against the entry for this
//! platform in the baseline file. The chat and daemon budgets join when
//! those commands exist (M1).
//!
//! `scripts/check` runs it against `perf/baseline.json`; CI's `perf` job
//! runs it on the reference runner against main's baseline
//! (see `perf/README.md`).

mod budget;
mod sample;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use self::budget::{Baseline, Metrics};

/// Measures, checks and optionally records the baseline entry for this
/// platform. A breached budget is never recorded.
pub(crate) fn run(
    root: &Path,
    runs: usize,
    baseline_path: &Path,
    update: bool,
) -> Result<(), String> {
    let current = measure(root, runs)?;
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

/// Builds `name` with `cargo build <args>` and returns the executable cargo
/// reports, so `CARGO_TARGET_DIR`, `build.target-dir` and
/// `CARGO_BUILD_TARGET` all land on the right path.
fn cargo_build_bin(root: &Path, args: &[&str], name: &str) -> Result<PathBuf, String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .current_dir(root)
        .args([
            "build",
            "--locked",
            "--message-format=json-render-diagnostics",
        ])
        .args(args)
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("cargo build: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo build {} failed ({})",
            args.join(" "),
            output.status
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| bin_artifact(line, name))
        .ok_or_else(|| format!("cargo build reported no executable for {name}"))
}

/// The `executable` of a `compiler-artifact` message for binary `name`.
fn bin_artifact(line: &str, name: &str) -> Option<PathBuf> {
    let message: serde_json::Value = serde_json::from_str(line).ok()?;
    let target = &message["target"];
    let is_bin = target["kind"].as_array()?.iter().any(|kind| kind == "bin");
    if message["reason"] != "compiler-artifact" || target["name"] != name || !is_bin {
        return None;
    }
    message["executable"].as_str().map(PathBuf::from)
}

fn measure(root: &Path, runs: usize) -> Result<Metrics, String> {
    let binary = cargo_build_bin(
        root,
        &["--release", "--package", "prometheus"],
        "prometheus",
    )?;
    let sampler = cargo_build_bin(
        root,
        &["--package", "xtask", "--bin", "perf-sampler"],
        "perf-sampler",
    )?;
    let measured = sample::collect(&sampler, &binary, runs)?;
    let (startup_median_ms, startup_p95_ms) = budget::summarize(measured.startup_ms);
    let binary_bytes = std::fs::metadata(&binary)
        .map_err(|e| format!("{}: {e}", binary.display()))?
        .len();
    Ok(Metrics {
        startup_median_ms: round2(startup_median_ms),
        startup_p95_ms: round2(startup_p95_ms),
        peak_rss_bytes: measured.peak_rss_bytes,
        binary_bytes,
    })
}

fn report(platform: &str, current: &Metrics, stored: Option<&Metrics>) {
    let rss = current
        .peak_rss_bytes
        .map_or_else(|| "not measurable".to_owned(), budget::mib);
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::bin_artifact;

    #[test]
    fn the_executable_comes_from_the_named_bin_artifact() {
        let lib = r#"{"reason":"compiler-artifact","target":{"name":"prometheus","kind":["lib"]},"executable":null}"#;
        let bin = r#"{"reason":"compiler-artifact","target":{"name":"prometheus","kind":["bin"]},"executable":"/t/x86_64-unknown-linux-musl/release/prometheus"}"#;
        let other = r#"{"reason":"compiler-artifact","target":{"name":"xtask","kind":["bin"]},"executable":"/t/debug/xtask"}"#;
        assert_eq!(bin_artifact(lib, "prometheus"), None);
        assert_eq!(bin_artifact(other, "prometheus"), None);
        assert_eq!(bin_artifact("not json", "prometheus"), None);
        assert_eq!(
            bin_artifact(bin, "prometheus"),
            Some(PathBuf::from(
                "/t/x86_64-unknown-linux-musl/release/prometheus"
            ))
        );
    }
}
