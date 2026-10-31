//! Carving credential stores out of a granted root, for sandboxes that can
//! only grant (Landlock, `AppContainer` ACL entries).
//!
//! A root with no denied store below it is granted whole. Otherwise the
//! root itself may only be listed, and each entry is carved in turn: stores
//! are skipped, entries with a store further down recurse, the rest are
//! granted whole. Entries created later in a listed-only directory are
//! therefore not reachable, and a store's name stays visible in a listing.

use std::path::{Path, PathBuf};

use crate::policy::is_strictly_below;

/// How far a carved grant reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reach {
    /// The path and everything below it.
    Subtree,
    /// Listing the directory only (no file contents, nothing inherited).
    ListOnly,
}

/// The grants that cover `root` minus every `denied` path inside it.
pub(crate) fn carve(root: &Path, denied: &[&Path]) -> Vec<(PathBuf, Reach)> {
    let mut grants = Vec::new();
    carve_into(root, denied, &mut grants);
    grants
}

fn carve_into(root: &Path, denied: &[&Path], grants: &mut Vec<(PathBuf, Reach)>) {
    let inside: Vec<&Path> = denied
        .iter()
        .copied()
        .filter(|d| is_strictly_below(d, root))
        .collect();
    if inside.is_empty() {
        grants.push((root.to_path_buf(), Reach::Subtree));
        return;
    }
    grants.push((root.to_path_buf(), Reach::ListOnly));
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !inside.iter().any(|d| path.starts_with(d)) {
            carve_into(&path, &inside, grants);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carving_skips_the_store_and_keeps_its_siblings() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp");
        std::fs::create_dir_all(&base).unwrap();
        let dir = tempfile::tempdir_in(base).unwrap();
        let home = std::fs::canonicalize(dir.path()).unwrap();
        for sub in [".ssh", ".config/gcloud", ".config/other", "notes"] {
            std::fs::create_dir_all(home.join(sub)).unwrap();
        }
        let denied = [home.join(".ssh"), home.join(".config").join("gcloud")];
        let denied: Vec<&Path> = denied.iter().map(PathBuf::as_path).collect();
        let grants = carve(&home, &denied);
        let reach = |p: PathBuf| grants.iter().find(|(g, _)| *g == p).map(|(_, r)| *r);
        assert_eq!(reach(home.clone()), Some(Reach::ListOnly));
        assert_eq!(reach(home.join(".config")), Some(Reach::ListOnly));
        assert_eq!(reach(home.join("notes")), Some(Reach::Subtree));
        assert_eq!(
            reach(home.join(".config").join("other")),
            Some(Reach::Subtree)
        );
        assert!(!grants.iter().any(|(p, _)| p.starts_with(home.join(".ssh"))));
        assert!(
            !grants
                .iter()
                .any(|(p, _)| p.starts_with(home.join(".config").join("gcloud")))
        );
        assert_eq!(
            carve(&home.join("notes"), &denied),
            [(home.join("notes"), Reach::Subtree)]
        );
    }
}
