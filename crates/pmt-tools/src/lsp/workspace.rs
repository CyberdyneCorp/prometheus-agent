//! Workspace gating and trust (spec: lsp-diagnostics, Workspace gating;
//! Workspace trust).
//!
//! A server runs for an edited file only inside a git working tree whose
//! root `lsp.exclude_roots` does not match. Trust is decided per git root:
//! the working tree of the directory the session was pointed at, or a
//! root under an `lsp.trusted_workspaces` entry. Trust is fixed when the
//! session starts: a `cd` in the terminal does not move it, a nested
//! checkout with its own `.git` is a root of its own, and a repository at
//! or above the user's home never inherits the session's trust.

use std::path::{Path, PathBuf};

/// The git working tree containing `path` (the nearest ancestor holding
/// a `.git` directory or file).
#[must_use]
pub fn git_root(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .skip_while(|p| !p.is_dir())
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// The directory a server for `file` starts in: the outermost ancestor up
/// to `root` holding one of `markers` (a Cargo or npm workspace's top, not
/// a member), else `root`.
#[must_use]
pub fn server_root(file: &Path, root: &Path, markers: &[String]) -> PathBuf {
    if markers.is_empty() {
        return root.to_path_buf();
    }
    file.ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(root))
        .filter(|dir| markers.iter().any(|m| dir.join(m).exists()))
        .last()
        .unwrap_or(root)
        .to_path_buf()
}

/// Roots as `lsp.exclude_roots` / `lsp.trusted_workspaces` list them.
#[derive(Debug, Clone, Default)]
pub struct Roots {
    patterns: Vec<(PathBuf, Option<glob::Pattern>)>,
}

impl Roots {
    /// `entries` with `~` expanded against `user_home`.
    #[must_use]
    pub fn new(entries: &[String], user_home: Option<&Path>) -> Self {
        let patterns = entries
            .iter()
            .map(|entry| {
                let path = expand(entry, user_home);
                let text = path.to_string_lossy().into_owned();
                let glob = text
                    .contains(['*', '?', '['])
                    .then(|| glob::Pattern::new(&text).ok())
                    .flatten();
                (path, glob)
            })
            .collect();
        Self { patterns }
    }

    /// Whether `root` matches an entry: a glob matches the root itself, a
    /// bare path matches it and everything below it.
    #[must_use]
    pub fn matches(&self, root: &Path) -> bool {
        self.patterns.iter().any(|(path, glob)| match glob {
            Some(glob) => glob.matches_path(root),
            None => root.starts_with(path) || canonical(path).is_some_and(|p| root.starts_with(p)),
        })
    }
}

fn canonical(path: &Path) -> Option<PathBuf> {
    dunce::canonicalize(path).ok()
}

/// `~` and `~/...` against `home`.
fn expand(entry: &str, home: Option<&Path>) -> PathBuf {
    match (entry.strip_prefix('~'), home) {
        (Some(""), Some(home)) => home.to_path_buf(),
        (Some(rest), Some(home)) if rest.starts_with(['/', '\\']) => home.join(&rest[1..]),
        _ => PathBuf::from(entry),
    }
}

/// Where a session's servers may run and which of them may start.
#[derive(Debug, Clone)]
pub struct Workspaces {
    excluded: Roots,
    trusted: Roots,
    /// The git root of the directory the session was pointed at, when it
    /// earns automatic trust.
    session_root: Option<PathBuf>,
}

/// Why a file gets no server, or how it is trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Not inside a git working tree.
    NotARepository,
    /// The root matches `lsp.exclude_roots`.
    Excluded(PathBuf),
    /// A root the session may run servers in.
    Root {
        /// The git root.
        root: PathBuf,
        /// Whether servers that execute project code may start.
        trusted: bool,
    },
}

/// What a session's trust starts from.
#[derive(Debug, Clone)]
pub struct TrustInputs<'a> {
    /// The directory the session was pointed at.
    pub start_dir: &'a Path,
    /// Whether it earns automatic trust (not cron, not a kanban worker).
    pub automatic: bool,
    /// The user's home.
    pub user_home: Option<&'a Path>,
    /// `lsp.exclude_roots`.
    pub exclude_roots: &'a [String],
    /// `lsp.trusted_workspaces`.
    pub trusted_workspaces: &'a [String],
}

impl Workspaces {
    /// The gate of a session started as `inputs` says.
    #[must_use]
    pub fn new(inputs: &TrustInputs<'_>) -> Self {
        let start = canonical(inputs.start_dir).unwrap_or_else(|| inputs.start_dir.to_path_buf());
        let home = inputs
            .user_home
            .map(|h| canonical(h).unwrap_or_else(|| h.to_path_buf()));
        let session_root = git_root(&start)
            .filter(|_| inputs.automatic)
            .filter(|root| home.as_deref().is_none_or(|home| !home.starts_with(root)));
        Self {
            excluded: Roots::new(inputs.exclude_roots, inputs.user_home),
            trusted: Roots::new(inputs.trusted_workspaces, inputs.user_home),
            session_root,
        }
    }

    /// The gate of an edited `file` (an absolute host path).
    #[must_use]
    pub fn gate(&self, file: &Path) -> Gate {
        let file = canonical_parent(file);
        let Some(root) = git_root(&file) else {
            return Gate::NotARepository;
        };
        if self.excluded.matches(&root) {
            return Gate::Excluded(root);
        }
        let trusted =
            self.session_root.as_deref() == Some(root.as_path()) || self.trusted.matches(&root);
        Gate::Root { root, trusted }
    }
}

/// `file` with its parent directory resolved (the file itself may be new).
fn canonical_parent(file: &Path) -> PathBuf {
    match (file.parent(), file.file_name()) {
        (Some(parent), Some(name)) => {
            canonical(parent).map_or_else(|| file.to_path_buf(), |p| p.join(name))
        }
        _ => file.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        dunce::canonicalize(dir).unwrap()
    }

    fn workspaces(start: &Path, home: &Path, automatic: bool, trusted: &[String]) -> Workspaces {
        Workspaces::new(&TrustInputs {
            start_dir: start,
            automatic,
            user_home: Some(home),
            exclude_roots: &["~/big".to_owned(), "/x/*/vendor".to_owned()],
            trusted_workspaces: trusted,
        })
    }

    #[test]
    fn the_session_root_is_trusted_and_other_checkouts_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let app = repo(&home.join("work/app"));
        let nested = repo(&app.join("vendor/lib"));
        let clone = repo(&dir.path().join("tmp/x"));
        std::fs::create_dir_all(app.join("src")).unwrap();
        let session = workspaces(&app.join("src"), &home, true, &[]);
        assert_eq!(
            session.gate(&app.join("src/main.rs")),
            Gate::Root {
                root: app.clone(),
                trusted: true
            }
        );
        assert_eq!(
            session.gate(&nested.join("lib.rs")),
            Gate::Root {
                root: nested,
                trusted: false
            }
        );
        assert_eq!(
            session.gate(&clone.join("build.rs")),
            Gate::Root {
                root: clone.clone(),
                trusted: false
            }
        );
        let cron = workspaces(&app, &home, false, &[]);
        assert!(matches!(
            cron.gate(&app.join("a.rs")),
            Gate::Root { trusted: false, .. }
        ));
        let explicit = workspaces(
            &app,
            &home,
            true,
            &[clone.parent().unwrap().display().to_string()],
        );
        assert!(matches!(
            explicit.gate(&clone.join("a.rs")),
            Gate::Root { trusted: true, .. }
        ));
    }

    #[test]
    fn home_is_not_a_workspace_and_a_dotfiles_repo_is_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let plain = workspaces(&home, &home, true, &[]);
        assert_eq!(plain.gate(&home.join("notes.py")), Gate::NotARepository);
        let home = repo(&home);
        let dotfiles = workspaces(&home, &home, true, &[]);
        assert!(matches!(
            dotfiles.gate(&home.join("notes.py")),
            Gate::Root { trusted: false, .. }
        ));
    }

    #[test]
    fn excluded_roots_match_bare_paths_below_and_globs() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let big = repo(&home.join("big/mono"));
        let session = workspaces(&big, &home, true, &[]);
        assert_eq!(session.gate(&big.join("a.rs")), Gate::Excluded(big.clone()));
        let roots = Roots::new(&["/x/*/vendor".to_owned()], None);
        assert!(roots.matches(Path::new("/x/a/vendor")));
        assert!(!roots.matches(Path::new("/x/a/vendor/sub")));
    }

    #[test]
    fn the_server_root_is_the_outermost_marker_inside_the_repository() {
        let dir = tempfile::tempdir().unwrap();
        let root = repo(&dir.path().join("repo"));
        std::fs::create_dir_all(root.join("tools/ws/crates/a/src")).unwrap();
        std::fs::write(root.join("tools/ws/crates/a/Cargo.toml"), "").unwrap();
        std::fs::write(root.join("tools/ws/Cargo.toml"), "").unwrap();
        let markers = vec!["Cargo.toml".to_owned()];
        assert_eq!(
            server_root(&root.join("tools/ws/crates/a/src/lib.rs"), &root, &markers),
            root.join("tools/ws")
        );
        assert_eq!(server_root(&root.join("x.rs"), &root, &markers), root);
    }
}
