//! Tree archives (spec: git-worktrees, Archive format; Worktree CLI and
//! restore).
//!
//! An archive is the commit `refs/prometheus/archive/<tree>` shaped like
//! `git stash -u`: its tree is the whole working state (tracked and
//! non-ignored untracked files), its first parent the tree's `HEAD`, its
//! second parent a commit of the staged state. It is built in a temporary
//! index from file bytes hashed with `--no-filters`, so neither the tree
//! nor its index is ever modified and no attribute, line-ending or filter
//! conversion touches the bytes; a restore writes the blobs back directly.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::git::{Git, git_error, split_nul};
use crate::repo::Repo;
use crate::tree::{self, Tree};

/// Where archives live.
pub const PREFIX: &str = "refs/prometheus/archive/";

/// The archive commit's subject prefix; the JSON metadata follows it.
const SUBJECT: &str = "prometheus worktree archive: ";

/// What an archive records besides the files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveMeta {
    /// The tree's name.
    pub name: String,
    /// Its path when archived.
    pub path: String,
    /// Its branch (`None`: detached).
    pub branch: Option<String>,
    /// Its `HEAD` when archived.
    pub head: String,
    /// Unix seconds.
    pub archived: u64,
    /// Why it was archived (`sweep`, `exit`, `subagent`...).
    pub reason: String,
    /// Untracked files left out for their size (`path (bytes)`).
    #[serde(default)]
    pub omitted: Vec<String>,
}

/// A saved archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archive {
    /// `refs/prometheus/archive/<name>`.
    pub reference: String,
    /// The archive commit.
    pub commit: String,
    /// Its metadata.
    pub meta: ArchiveMeta,
}

/// The outcome of archiving a tree.
#[derive(Debug, Clone)]
pub struct Archived {
    /// The saved archive.
    pub archive: Archive,
    /// The tree changed while the archive was built (its `HEAD` or index
    /// moved): the archive is saved but the tree must be kept.
    pub changed: bool,
}

/// What identifies a tree's state between the start and end of a build.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    head: String,
    index: [u8; 32],
}

fn fingerprint(git: &Git, tree: &Tree) -> Result<Fingerprint, Error> {
    let head = git.run(&["rev-parse", "--verify", "HEAD"])?;
    let index = std::fs::read(tree.admin.join("index")).unwrap_or_default();
    Ok(Fingerprint {
        head,
        index: Sha256::digest(&index).into(),
    })
}

/// Archives `tree` (untracked files over `max_file_bytes` are left out and
/// listed); `between` runs after the archive is built and before the
/// tree is checked for concurrent changes.
///
/// # Errors
///
/// `worktree.git` when the archive cannot be built (an embedded
/// repository, an unmerged index, a git failure): the tree must be kept.
pub fn archive_with(
    repo: &Repo,
    tree: &Tree,
    reason: &str,
    max_file_bytes: u64,
    between: &mut dyn FnMut(),
) -> Result<Archived, Error> {
    let git = repo.git_in(&tree.path)?;
    let before = fingerprint(&git, tree)?;
    let scratch = tree.admin.join("prometheus-archive.index");
    let built = build(
        repo,
        tree,
        &git,
        &scratch,
        &before.head,
        reason,
        max_file_bytes,
    );
    let _ = std::fs::remove_file(&scratch);
    let archive = built?;
    between();
    let after = fingerprint(&git, tree)?;
    Ok(Archived {
        archive,
        changed: after != before,
    })
}

/// [`archive_with`] without a hook.
///
/// # Errors
///
/// As [`archive_with`].
pub fn archive(
    repo: &Repo,
    tree: &Tree,
    reason: &str,
    max_file_bytes: u64,
) -> Result<Archived, Error> {
    archive_with(repo, tree, reason, max_file_bytes, &mut || {})
}

#[allow(clippy::too_many_arguments)]
fn build(
    repo: &Repo,
    tree: &Tree,
    git: &Git,
    scratch: &Path,
    head: &str,
    reason: &str,
    max_file_bytes: u64,
) -> Result<Archive, Error> {
    let io = |e: std::io::Error| git_error(&["archive"], &e.to_string());
    match std::fs::copy(tree.admin.join("index"), scratch) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(e)),
    }
    let temp = git.with_index(scratch);
    let staged_tree = temp.run(&["write-tree"])?;
    let identity = identity(&temp);
    let staged = identity.run(&["commit-tree", &staged_tree, "-p", head, "-m", "staged"])?;
    let omitted = stage_working_files(&temp, &tree.path, max_file_bytes)?;
    let working_tree = temp.run(&["write-tree"])?;
    let meta = ArchiveMeta {
        name: tree.meta.name.clone(),
        path: tree.path.to_string_lossy().into_owned(),
        branch: tree.branch(),
        head: head.to_owned(),
        archived: tree::now_secs(),
        reason: reason.to_owned(),
        omitted,
    };
    let message = format!(
        "{SUBJECT}{}\n\n{}\n",
        meta.name,
        serde_json::to_string(&meta).map_err(|e| git_error(&["archive"], &e.to_string()))?
    );
    let commit = identity.run(&[
        "commit-tree",
        &working_tree,
        "-p",
        head,
        "-p",
        &staged,
        "-m",
        &message,
    ])?;
    let reference = format!("{PREFIX}{}", meta.name);
    repo.git()?.run(&["update-ref", &reference, &commit])?;
    Ok(Archive {
        reference,
        commit,
        meta,
    })
}

/// A runner that commits as the archiver (never the user's identity or
/// signing setup).
fn identity(git: &Git) -> Git {
    let mut git = git.clone();
    for (key, value) in [
        ("GIT_AUTHOR_NAME", "Prometheus worktree archive"),
        ("GIT_AUTHOR_EMAIL", "prometheus-archive@localhost"),
        ("GIT_COMMITTER_NAME", "Prometheus worktree archive"),
        ("GIT_COMMITTER_EMAIL", "prometheus-archive@localhost"),
    ] {
        git = git.with_env(key, value.into());
    }
    git
}

/// Puts the bytes of every changed tracked and every non-ignored
/// untracked file into the temporary index; returns the untracked files
/// left out for their size.
fn stage_working_files(temp: &Git, root: &Path, max_file_bytes: u64) -> Result<Vec<String>, Error> {
    let changed = split_nul(&temp.run_bytes(&["diff-files", "-z", "--name-only"], None)?);
    let untracked =
        split_nul(&temp.run_bytes(&["ls-files", "-z", "--others", "--exclude-standard"], None)?);
    if let Some(repo) = untracked.iter().find(|p| p.ends_with('/')) {
        return Err(git_error(
            &["archive"],
            &format!("{repo} is a repository of its own and cannot be archived"),
        ));
    }
    let untracked_set: HashSet<&String> = untracked.iter().collect();
    let mut index_info = Vec::new();
    let mut omitted = Vec::new();
    let mut seen = HashSet::new();
    for path in changed.iter().chain(untracked.iter()) {
        if !seen.insert(path) {
            continue;
        }
        let full = root.join(path);
        let Ok(meta) = std::fs::symlink_metadata(&full) else {
            // Deleted in the working tree.
            index_info.extend_from_slice(format!("0 {}\t{path}\0", "0".repeat(40)).as_bytes());
            continue;
        };
        if untracked_set.contains(path) && meta.len() > max_file_bytes {
            omitted.push(format!("{path} ({} bytes)", meta.len()));
            continue;
        }
        let (mode, sha) = hash_file(temp, &full, &meta)?;
        index_info.extend_from_slice(format!("{mode} {sha}\t{path}\0").as_bytes());
    }
    if !index_info.is_empty() {
        temp.run_bytes(&["update-index", "-z", "--index-info"], Some(&index_info))?;
    }
    Ok(omitted)
}

/// Stores one file's exact bytes (a symlink's target) as a blob.
fn hash_file(
    git: &Git,
    path: &Path,
    meta: &std::fs::Metadata,
) -> Result<(&'static str, String), Error> {
    if meta.file_type().is_symlink() {
        let target =
            std::fs::read_link(path).map_err(|e| git_error(&["archive"], &e.to_string()))?;
        let sha = git.run_bytes(
            &["hash-object", "-w", "--no-filters", "--stdin"],
            Some(target.to_string_lossy().as_bytes()),
        )?;
        return Ok(("120000", String::from_utf8_lossy(&sha).trim().to_owned()));
    }
    let bytes = std::fs::read(path).map_err(|e| git_error(&["archive"], &e.to_string()))?;
    let sha = git.run_bytes(
        &["hash-object", "-w", "--no-filters", "--stdin"],
        Some(&bytes),
    )?;
    Ok((
        if executable(meta) { "100755" } else { "100644" },
        String::from_utf8_lossy(&sha).trim().to_owned(),
    ))
}

#[cfg(unix)]
fn executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &std::fs::Metadata) -> bool {
    false
}

/// The archive `name` of `repo`.
///
/// # Errors
///
/// `worktree.archive_not_found` when none exists.
pub fn find(repo: &Repo, name: &str) -> Result<Archive, Error> {
    list(repo)?
        .into_iter()
        .find(|a| a.meta.name == name)
        .ok_or_else(|| {
            Error::new(
                &codes::WORKTREE_ARCHIVE_NOT_FOUND,
                format!("no archive named '{name}' in {}", repo.root.display()),
            )
        })
}

/// Every archive of `repo`.
///
/// # Errors
///
/// `worktree.git`.
pub fn list(repo: &Repo) -> Result<Vec<Archive>, Error> {
    let git = repo.git()?;
    let refs = git.run(&[
        "for-each-ref",
        "--format=%(refname)%00%(objectname)",
        PREFIX,
    ])?;
    let mut archives = Vec::new();
    for line in refs.lines() {
        let Some((reference, commit)) = line.split_once('\0') else {
            continue;
        };
        let body = git.run(&["log", "-1", "--format=%B", commit])?;
        let Some(meta) = body
            .lines()
            .find_map(|l| serde_json::from_str::<ArchiveMeta>(l).ok())
        else {
            continue;
        };
        archives.push(Archive {
            reference: reference.to_owned(),
            commit: commit.to_owned(),
            meta,
        });
    }
    Ok(archives)
}

/// Deletes archive `archive`.
///
/// # Errors
///
/// `worktree.git`.
pub fn delete(repo: &Repo, archive: &Archive) -> Result<(), Error> {
    repo.git()?
        .run(&["update-ref", "-d", &archive.reference, &archive.commit])
        .map(|_| ())
}

/// A restored tree.
#[derive(Debug, Clone)]
pub struct Restored {
    /// Its directory.
    pub path: PathBuf,
    /// The branch it is on (`None`: detached at the archived `HEAD`).
    pub branch: Option<String>,
}

/// Recreates archive `name` at `path` (default `.worktrees/<name>`): its
/// branch when missing or still at the archived `HEAD` (else detached
/// there), its files byte for byte and its staged state.
///
/// # Errors
///
/// `worktree.archive_not_found`, `worktree.invalid_name` when `path`
/// exists, `worktree.git`.
pub fn restore(repo: &Repo, name: &str, path: Option<&Path>) -> Result<Restored, Error> {
    let archive = find(repo, name)?;
    let path = path.map_or_else(|| crate::create::path_of(repo, name), Path::to_path_buf);
    if path.exists() {
        return Err(Error::new(
            &codes::WORKTREE_INVALID_NAME,
            format!("{} already exists", path.display()),
        ));
    }
    let git = repo.git()?;
    let head = archive.meta.head.clone();
    let path_text = path.to_string_lossy().into_owned();
    let branch = archive.meta.branch.clone().filter(|b| {
        let current = git.run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{b}"),
        ]);
        current.map_or(true, |sha| sha == head)
    });
    match &branch {
        Some(b)
            if git.test(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{b}"),
            ]) =>
        {
            git.run(&["worktree", "add", &path_text, b])?;
        }
        Some(b) => {
            git.run(&["worktree", "add", "--no-track", "-b", b, &path_text, &head])?;
        }
        None => {
            git.run(&["worktree", "add", "--detach", &path_text, &head])?;
        }
    }
    let path = path.canonicalize().unwrap_or(path);
    let tree_git = repo.git_in(&path)?;
    write_working_files(&tree_git, &path, &head, &archive.commit)?;
    tree_git.run(&["read-tree", &format!("{}^2^{{tree}}", archive.commit)])?;
    if let Some(admin) = tree::admin_of(&path) {
        let meta = tree::Meta {
            name: name.to_owned(),
            kind: crate::names::Kind::of_name(name),
            branch: branch.clone().unwrap_or_default(),
            created: tree::now_secs(),
            last_decision: Some(format!("restored from {}", archive.reference)),
        };
        let _ = tree::write_meta(&admin, &meta);
    }
    Ok(Restored { path, branch })
}

/// Writes every file that differs between `head` and the archive's
/// working state, from the blobs' exact bytes.
fn write_working_files(git: &Git, root: &Path, head: &str, commit: &str) -> Result<(), Error> {
    let diff = git.run_bytes(
        &[
            "diff-tree",
            "-r",
            "-z",
            "--no-renames",
            "--raw",
            head,
            commit,
        ],
        None,
    )?;
    let fields = split_nul(&diff);
    let io = |e: std::io::Error| git_error(&["restore"], &e.to_string());
    for pair in fields.chunks(2) {
        let [info, path] = pair else { continue };
        // `:<old mode> <new mode> <old sha> <new sha> <status>`
        let parts: Vec<&str> = info.trim_start_matches(':').split(' ').collect();
        let [_, mode, _, sha, status] = parts.as_slice() else {
            continue;
        };
        let full = root.join(path);
        if *status == "D" {
            let _ = std::fs::remove_file(&full);
            continue;
        }
        let bytes = git.run_bytes(&["cat-file", "blob", sha], None)?;
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let _ = std::fs::remove_file(&full);
        write_entry(&full, mode, &bytes).map_err(io)?;
    }
    Ok(())
}

fn write_entry(path: &Path, mode: &str, bytes: &[u8]) -> std::io::Result<()> {
    if mode == "120000" {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(String::from_utf8_lossy(bytes).as_ref(), path);
    }
    std::fs::write(path, bytes)?;
    #[cfg(unix)]
    if mode == "100755" {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}
