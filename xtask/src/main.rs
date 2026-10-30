//! Repository automation, run as `cargo xtask <command>`.
//!
//! `cargo xtask gen` regenerates every checked-in artifact derived from the
//! types the binary uses, so no contract or default is written down twice:
//!
//! - `schema/config.schema.json`: JSON Schema of `config.toml` (`pmt-config`)
//! - `docs/reference/cli.md`: CLI reference (the `prometheus` clap tree)
//! - `docs/reference/error-codes.md`: exit codes, error kinds and error codes
//!   (`pmt-core`)
//!
//! `cargo xtask gen --check` writes nothing and exits 1 naming every stale
//! file. The RPC schema `schema/rpc.json` and its TypeScript bindings join
//! this list when the RPC catalog lands in M2 (tasks group 22).
//!
//! `cargo xtask health` enforces the complexity and unsafe-code ratchet
//! described in `health.toml`.

mod artifacts;
mod health;

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
}

fn main() -> ExitCode {
    let result = match Args::parse().command {
        Command::Gen { check } => generate(check),
        Command::Health { root, base } => {
            health::run(&root.unwrap_or_else(repo_root), base.as_deref())
        }
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
    use super::{Artifact, check_all, repo_root, write_all};

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
}
