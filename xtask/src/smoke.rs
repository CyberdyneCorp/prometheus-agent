//! `cargo xtask smoke` (spec: replay-and-evaluation, Built-in smoke
//! suite): builds the binary and runs its bundled `smoke` suite with
//! `prometheus smoke-suite` in `target/smoke/` (kept for inspection). The
//! binary grades every task itself, so the Tier 2 targets run the very
//! same suite without cargo.

use std::path::Path;
use std::process::Command;

pub(crate) fn run(root: &Path) -> Result<(), String> {
    let bin = crate::goldens::binary(root)?;
    let scratch = root.join("target/smoke");
    if scratch.exists() {
        std::fs::remove_dir_all(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
    }
    let mut cmd = Command::new(&bin);
    for (var, _) in std::env::vars_os() {
        let name = var.to_string_lossy();
        if name.starts_with("PMT_") || name.starts_with("PROMETHEUS_") {
            cmd.env_remove(&var);
        }
    }
    let status = cmd
        .args(["smoke-suite", "--keep", "--output"])
        .arg(&scratch)
        .status()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    if status.success() {
        return Ok(());
    }
    Err(format!(
        "the smoke suite failed ({status}); the tasks' homes are in {}",
        scratch.display()
    ))
}
