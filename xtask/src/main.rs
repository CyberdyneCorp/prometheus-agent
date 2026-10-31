//! Repository automation, run as `cargo xtask <command>`.
//!
//! `cargo xtask gen` regenerates every checked-in artifact derived from the
//! types the binary uses, so no contract or default is written down twice:
//!
//! - `schema/config.schema.json`: JSON Schema of `config.toml` (`pmt-config`)
//! - `docs/reference/configuration.md`: configuration reference (every key,
//!   type, default and allowed values, rendered from that schema)
//! - `schema/daemon.schema.json` and `docs/reference/daemon-configuration.md`:
//!   the same for the machine-wide `daemon.toml`
//! - `docs/reference/cli.md`: CLI reference (the `prometheus` clap tree and
//!   the registered `--json` output shapes)
//! - `docs/reference/error-codes.md`: exit codes, error kinds and error codes
//!   (`pmt-core`)
//! - `docs/reference/tools.md`: the shipped toolsets and every tool name they
//!   refer to (`pmt-tools`); links to it from any page under `docs/` must
//!   name a listed tool (both `gen` and `gen --check` refuse others)
//!
//! `cargo xtask gen --check` writes nothing and exits 1 naming every stale
//! file, and also fails on any file in `docs/reference/` it does not
//! generate, so the documentation site's reference pages are generated
//! only. The RPC schema `schema/rpc.json` and its TypeScript bindings join
//! this list when the RPC catalog lands in M2 (tasks group 22).
//!
//! `cargo xtask health` enforces the complexity and unsafe-code ratchet
//! described in `health.toml`.
//!
//! `cargo xtask perf` checks the startup, peak-RSS and binary-size budgets
//! against `perf/baseline.json`.
//!
//! `cargo xtask release <step>` runs the release steps around cargo-dist:
//! SBOMs, the signed manifest and channel index, rendered installers,
//! minisign signatures and the end-to-end check of a release directory.

mod artifacts;
mod health;
mod perf;
mod release;
mod tool_refs;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::artifacts::Artifact;

#[derive(Parser)]
#[command(name = "cargo xtask", bin_name = "cargo xtask")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Regenerate the checked-in contracts and reference docs.
    Gen {
        /// Compare instead of writing; exit 1 when any artifact is stale.
        #[arg(long)]
        check: bool,
    },
    /// Check functions and files against the complexity, size and unsafe
    /// budgets and the exceptions in `health.toml`.
    Health {
        /// Repository root to check (default: this workspace).
        #[arg(long)]
        root: Option<PathBuf>,
        /// The `health.toml` of the merge base; exceptions added or raised
        /// since then fail.
        #[arg(long)]
        base: Option<PathBuf>,
    },
    /// Measure startup time, peak RSS and release binary size against the
    /// budgets and the stored baseline.
    Perf {
        /// Timed `prometheus --version` runs (after 3 warm-up runs).
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u16).range(1..))]
        runs: u16,
        /// Baseline file (default: `perf/baseline.json`).
        #[arg(long)]
        baseline: Option<PathBuf>,
        /// Record this run as the baseline for this platform instead of
        /// comparing with it; the absolute budgets still apply.
        #[arg(long)]
        update_baseline: bool,
    },
    /// Release steps run by the Release workflow around cargo-dist.
    Release {
        #[command(subcommand)]
        step: release::Step,
    },
}

fn main() -> ExitCode {
    let result = match Args::parse().command {
        Command::Gen { check } => generate(check),
        Command::Health { root, base } => {
            health::run(&root.unwrap_or_else(repo_root), base.as_deref())
        }
        Command::Perf {
            runs,
            baseline,
            update_baseline,
        } => {
            let root = repo_root();
            let baseline = baseline.unwrap_or_else(|| root.join("perf/baseline.json"));
            perf::run(&root, runs.into(), &baseline, update_baseline)
        }
        Command::Release { step } => release::run(step),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn generate(check: bool) -> Result<(), String> {
    let root = repo_root();
    let artifacts = artifacts::all();
    reject_unowned(&root, artifacts::REFERENCE_DIR, &artifacts)?;
    tool_refs::check(&root, &artifacts)?;
    if check {
        check_all(&root, &artifacts)
    } else {
        write_all(&root, &artifacts)
    }
}

/// The workspace root: xtask always lives one level below it.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives inside the workspace")
        .to_path_buf()
}

fn write_all(root: &Path, artifacts: &[Artifact]) -> Result<(), String> {
    for artifact in artifacts {
        let path = root.join(artifact.path);
        let io_error = |e: std::io::Error| format!("{}: {e}", artifact.path);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io_error)?;
        }
        std::fs::write(&path, &artifact.contents).map_err(io_error)?;
        println!("wrote {}", artifact.path);
    }
    Ok(())
}

/// Fails when `dir` holds a file no artifact generates.
fn reject_unowned(root: &Path, dir: &str, artifacts: &[Artifact]) -> Result<(), String> {
    let entries = std::fs::read_dir(root.join(dir)).map_err(|e| format!("{dir}: {e}"))?;
    let mut unowned: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|entry| format!("{dir}/{}", entry.file_name().to_string_lossy()))
        .filter(|path| !artifacts.iter().any(|a| a.path == path))
        .collect();
    if unowned.is_empty() {
        return Ok(());
    }
    unowned.sort();
    for path in &unowned {
        eprintln!("not generated: {path}");
    }
    Err(format!(
        "{dir}/ holds only generated files; move hand-written pages elsewhere in docs/"
    ))
}

fn check_all(root: &Path, artifacts: &[Artifact]) -> Result<(), String> {
    let stale: Vec<&str> = artifacts
        .iter()
        .filter(|a| std::fs::read_to_string(root.join(a.path)).ok().as_deref() != Some(&a.contents))
        .map(|a| a.path)
        .collect();
    if stale.is_empty() {
        return Ok(());
    }
    for path in &stale {
        eprintln!("stale: {path}");
    }
    Err(format!(
        "{} generated file(s) differ from a fresh generation; run `cargo xtask gen`",
        stale.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::{Artifact, check_all, reject_unowned, repo_root, write_all};

    #[test]
    fn check_flags_only_files_that_differ_from_generation() {
        let dir = repo_root().join("target/xtask-test/check");
        let _ = std::fs::remove_dir_all(&dir);
        let artifacts = [
            Artifact {
                path: "a.md",
                contents: "fresh\n".into(),
            },
            Artifact {
                path: "b.md",
                contents: "fresh\n".into(),
            },
        ];
        write_all(&dir, &artifacts).unwrap();
        assert!(check_all(&dir, &artifacts).is_ok());

        std::fs::write(dir.join("b.md"), "hand edit\n").unwrap();
        let err = check_all(&dir, &artifacts).unwrap_err();
        assert!(err.starts_with("1 generated file(s)"), "{err}");
    }

    #[test]
    fn a_hand_written_file_among_generated_ones_is_rejected() {
        let dir = repo_root().join("target/xtask-test/unowned");
        let _ = std::fs::remove_dir_all(&dir);
        let artifacts = [Artifact {
            path: "ref/gen.md",
            contents: "fresh\n".into(),
        }];
        write_all(&dir, &artifacts).unwrap();
        assert!(reject_unowned(&dir, "ref", &artifacts).is_ok());

        std::fs::write(dir.join("ref/manual.md"), "hand written\n").unwrap();
        assert!(reject_unowned(&dir, "ref", &artifacts).is_err());
    }
}
