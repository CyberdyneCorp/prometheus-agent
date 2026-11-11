//! `cargo xtask perf`: the startup, memory and size gate
//! (spec: workspace-architecture, Startup time budget; Memory and size
//! budgets).
//!
//! It builds the release binary, runs `prometheus --version` repeatedly from
//! the small `perf-sampler` binary, and fails when a measurement exceeds its
//! absolute budget or regresses more than 15% against the entry for this
//! platform in the baseline file (growth under small noise/size floors never
//! counts). It also starts `prometheus chat` with the mock provider in a
//! pseudo-terminal and times its prompt, and measures the peak RSS of chat
//! starts and `chat -z` runs (Unix). The daemon budgets join with the
//! daemon (M2).
//!
//! `scripts/check` runs it against `perf/baseline.json`; CI's `perf` job
//! runs it on the reference runner against the pull request's base branch,
//! measured in the same job
//! (see `perf/README.md`).

mod budget;
mod overhead;
mod pair;
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
    base_binary: Option<&Path>,
) -> Result<(), String> {
    let (current, base) = measure(root, runs, base_binary)?;
    let mut baseline = load_baseline(baseline_path)?;
    let platform = platform();
    if let Some(base) = &base {
        report(&format!("{platform}, base binary"), base, Some(base));
    }
    let reference = base.as_ref().or_else(|| baseline.get(&platform));
    // Reported before the overhead gate, which may fail first.
    report(&platform, &current, reference);
    check_overhead(root)?;
    let stored = if update { None } else { reference };

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

/// The observability overhead gate on the release binary.
fn check_overhead(root: &Path) -> Result<(), String> {
    let binary = cargo_build_bin(
        root,
        &["--release", "--package", "prometheus"],
        "prometheus",
    )?;
    let tasks = overhead::tasks(root)?;
    let measured = overhead::measure(&binary, &root.join("target/perf-overhead"), &tasks)?;
    println!(
        "perf: smoke suite {:.3}s with recording off, {:.3}s with recording and logging, {:.3}s with OTLP export (sum of each task's median of {})",
        measured.off,
        measured.recording,
        measured.export,
        overhead::RUNS
    );
    let violations = overhead::evaluate(&measured);
    for violation in &violations {
        eprintln!("perf: {violation}");
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} observability overhead violation(s)",
            violations.len()
        ))
    }
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

/// Builds the release binary and measures it, and `base` (another
/// release binary) alternately with it when given.
fn measure(
    root: &Path,
    runs: usize,
    base: Option<&Path>,
) -> Result<(Metrics, Option<Metrics>), String> {
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
    pair::measure(root, &sampler, &binary, base, runs)
}

fn report(platform: &str, current: &Metrics, stored: Option<&Metrics>) {
    let rss = |bytes: Option<u64>| bytes.map_or_else(|| "not measurable".to_owned(), budget::mib);
    println!(
        "perf ({platform}): startup median {}, p95 {}; peak RSS {}; binary {}",
        budget::ms(current.startup_median_ms),
        budget::ms(current.startup_p95_ms),
        rss(current.peak_rss_bytes),
        budget::mib(current.binary_bytes),
    );
    if let (Some(median), Some(p95)) = (current.chat_prompt_median_ms, current.chat_prompt_p95_ms) {
        println!(
            "perf ({platform}): chat prompt median {}, p95 {}; chat peak RSS {}",
            budget::ms(median),
            budget::ms(p95),
            rss(current.chat_peak_rss_bytes),
        );
    }
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
