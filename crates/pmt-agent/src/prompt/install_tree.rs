//! Install-tree context suppression (spec: agent-turn-loop, Install-tree
//! context suppression).
//!
//! A working directory chosen by fallback (no configured or explicit one)
//! that lies inside the Prometheus installation or source tree would load
//! Prometheus's own `AGENTS.md` as if it were the user's project, so
//! project-context discovery is skipped there.

use std::path::{Path, PathBuf};

/// The directories that make up this build's installation and source tree:
/// the executable's directory (its parent when that is a `bin`
/// directory), the checkout holding a cargo `target` directory the
/// executable runs from, and the workspace this build was compiled from.
#[must_use]
pub fn roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.canonicalize().ok())
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        roots.extend(exe_roots(&dir));
    }
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    if source.join("Cargo.toml").is_file()
        && let Ok(source) = source.canonicalize()
    {
        roots.push(source);
    }
    roots
}

/// The installation roots implied by the executable's directory.
fn exe_roots(dir: &Path) -> Vec<PathBuf> {
    let mut roots = vec![dir.to_path_buf()];
    if dir.file_name().is_some_and(|n| n == "bin")
        && let Some(prefix) = dir.parent()
    {
        roots.push(prefix.to_path_buf());
    }
    if let Some(checkout) = dir
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "target"))
        .and_then(Path::parent)
    {
        roots.push(checkout.to_path_buf());
    }
    roots
}

/// Whether `cwd` lies inside one of `roots`.
#[must_use]
pub fn contains(roots: &[PathBuf], cwd: &Path) -> bool {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    roots.iter().any(|root| cwd.starts_with(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cargo_checkout_and_a_bin_prefix_are_install_roots() {
        let roots = exe_roots(Path::new("/src/prometheus/target/debug"));
        assert!(roots.contains(&PathBuf::from("/src/prometheus")));
        let roots = exe_roots(Path::new("/opt/prometheus/bin"));
        assert!(roots.contains(&PathBuf::from("/opt/prometheus")));
        assert!(contains(&roots, Path::new("/opt/prometheus/share")));
        assert!(!contains(&roots, Path::new("/home/me/project")));
    }

    #[test]
    fn this_build_knows_its_source_tree() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(contains(&roots(), here));
    }
}
