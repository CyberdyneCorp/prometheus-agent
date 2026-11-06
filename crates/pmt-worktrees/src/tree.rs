//! Prometheus trees of a repository: linked worktrees under `.worktrees/`
//! whose git admin directory (`<common>/worktrees/<id>/`) carries the
//! `prometheus.json` marker written at creation. Trees without the marker
//! (made by hand or by another tool) are never listed, swept or removed.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::names::Kind;
use crate::repo::Repo;

/// The marker file in a tree's admin directory.
pub const MARKER: &str = "prometheus.json";
/// The live-owner lock in a tree's admin directory.
pub const LOCK: &str = "prometheus.lock";

/// What Prometheus recorded about a tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    /// The tree's name (its directory under `.worktrees/`).
    pub name: String,
    /// Who named it (its idle tier).
    pub kind: Kind,
    /// Its branch at creation.
    pub branch: String,
    /// Creation time, Unix seconds.
    pub created: u64,
    /// The last sweep decision and its reason (`worktrees list`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_decision: Option<String>,
}

/// One Prometheus tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    /// Its checkout directory.
    pub path: PathBuf,
    /// Its admin directory (`<common>/worktrees/<id>`).
    pub admin: PathBuf,
    /// What was recorded at creation.
    pub meta: Meta,
}

impl Tree {
    /// The tree's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.meta.name
    }

    /// The branch its `HEAD` is on now (`None`: detached or unreadable).
    #[must_use]
    pub fn branch(&self) -> Option<String> {
        let head = std::fs::read_to_string(self.admin.join("HEAD")).ok()?;
        head.trim()
            .strip_prefix("ref: refs/heads/")
            .map(str::to_owned)
    }

    /// Whether its checkout directory still exists.
    #[must_use]
    pub fn exists(&self) -> bool {
        self.path.join(".git").exists()
    }

    /// Records the sweep's decision on the tree's marker.
    pub fn record_decision(&mut self, decision: &str) {
        self.meta.last_decision = Some(decision.to_owned());
        if let Err(err) = write_meta(&self.admin, &self.meta) {
            tracing::debug!(error = %err, tree = %self.path.display(), "decision not recorded");
        }
    }
}

/// Writes `meta` into the admin directory.
///
/// # Errors
///
/// The file cannot be written.
pub fn write_meta(admin: &Path, meta: &Meta) -> std::io::Result<()> {
    let text = serde_json::to_vec_pretty(meta).map_err(std::io::Error::other)?;
    pmt_core::private_fs::write_atomic(&admin.join(MARKER), &text)
}

/// The admin directory of the tree checked out at `path` (from its `.git`
/// file).
#[must_use]
pub fn admin_of(path: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(path.join(".git")).ok()?;
    let dir = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
    let dir = if dir.is_absolute() {
        dir
    } else {
        path.join(dir)
    };
    Some(dir.canonicalize().unwrap_or(dir))
}

/// What git in a linked worktree touches outside the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedGitDirs {
    /// The common git directory (read: configuration, `HEAD`s).
    pub common: PathBuf,
    /// What a commit writes: the shared `objects`, `refs` and `logs` and
    /// the tree's own admin directory. Hooks and configuration are
    /// deliberately not among them.
    pub writes: Vec<PathBuf>,
}

/// The git directories of the linked worktree at `dir`; `None` for a main
/// checkout (its git directory is inside it) or outside git.
#[must_use]
pub fn shared_git_dirs(dir: &Path) -> Option<SharedGitDirs> {
    let mut top = dir.canonicalize().ok()?;
    while !top.join(".git").exists() {
        top = top.parent()?.to_path_buf();
    }
    if top.join(".git").is_dir() {
        return None;
    }
    let admin = admin_of(&top)?;
    let common = admin.parent()?.parent()?;
    if admin.parent()?.file_name()? != "worktrees" {
        return None;
    }
    Some(SharedGitDirs {
        common: common.to_path_buf(),
        writes: vec![
            common.join("objects"),
            common.join("refs"),
            common.join("logs"),
            admin.clone(),
        ],
    })
}

/// Every Prometheus tree of `repo`, by name.
#[must_use]
pub fn list(repo: &Repo) -> Vec<Tree> {
    let Ok(entries) = std::fs::read_dir(repo.common.join("worktrees")) else {
        return Vec::new();
    };
    let mut trees: Vec<Tree> = entries
        .flatten()
        .filter_map(|entry| read(repo, &entry.path()))
        .collect();
    trees.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
    trees
}

/// The Prometheus tree whose admin directory is `admin`.
fn read(repo: &Repo, admin: &Path) -> Option<Tree> {
    let meta: Meta = serde_json::from_slice(&std::fs::read(admin.join(MARKER)).ok()?).ok()?;
    // `gitdir` names the tree's `.git` file; the tree is its parent.
    let gitdir = std::fs::read_to_string(admin.join("gitdir")).ok()?;
    let path = PathBuf::from(gitdir.trim()).parent()?.to_path_buf();
    let path = path.canonicalize().unwrap_or(path);
    path.starts_with(repo.trees_dir()).then(|| Tree {
        path,
        admin: admin.to_path_buf(),
        meta,
    })
}

/// The Prometheus tree of `repo` named `name`.
#[must_use]
pub fn find(repo: &Repo, name: &str) -> Option<Tree> {
    list(repo).into_iter().find(|t| t.meta.name == name)
}

/// The Prometheus tree containing `dir`, if any.
#[must_use]
pub fn containing(dir: &Path) -> Option<(Repo, Tree)> {
    // Cheap first: every Prometheus tree lives under a `.worktrees`
    // directory, so most sessions never start git here.
    if !dir
        .components()
        .any(|c| c.as_os_str() == crate::repo::TREES_DIR)
    {
        return None;
    }
    let repo = Repo::discover(dir).ok()?;
    let dir = dir.canonicalize().ok()?;
    let tree = list(&repo).into_iter().find(|t| dir.starts_with(&t.path))?;
    Some((repo, tree))
}

/// Now, in Unix seconds.
#[must_use]
pub fn now_secs() -> u64 {
    secs_of(SystemTime::now())
}

/// `time` in Unix seconds.
#[must_use]
pub fn secs_of(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Who holds a tree: a running Prometheus session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    /// The owning process.
    pub pid: u32,
    /// Its start time (a reused pid is not the owner).
    pub start_time: u64,
    /// The owning session, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

impl Owner {
    /// Whether the owning process still runs.
    #[must_use]
    pub fn alive(&self) -> bool {
        pmt_core::runtime::process_start_time(self.pid)
            .is_some_and(|start| self.start_time == 0 || start == self.start_time)
    }
}

/// The live owner of a tree while a session works in it; dropping it
/// releases the tree to the sweep.
#[derive(Debug)]
pub struct OwnerLock {
    path: PathBuf,
    session: Option<String>,
}

impl OwnerLock {
    /// Marks `tree` as owned by this process's `session`.
    ///
    /// # Errors
    ///
    /// The lock file cannot be written.
    pub fn take(tree: &Tree, session: Option<&str>) -> std::io::Result<Self> {
        let pid = std::process::id();
        let owner = Owner {
            pid,
            start_time: pmt_core::runtime::process_start_time(pid).unwrap_or(0),
            session: session.map(str::to_owned),
        };
        let path = tree.admin.join(LOCK);
        let text = serde_json::to_vec(&owner).map_err(std::io::Error::other)?;
        pmt_core::private_fs::write_atomic(&path, &text)?;
        Ok(Self {
            path,
            session: owner.session,
        })
    }
}

impl Drop for OwnerLock {
    fn drop(&mut self) {
        // Only this process's own lock is released: another session may
        // have taken the tree over since.
        let ours = std::fs::read(&self.path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Owner>(&b).ok())
            .is_some_and(|o| o.pid == std::process::id() && o.session == self.session);
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The tree's live owner, if one holds it.
#[must_use]
pub fn live_owner(tree: &Tree) -> Option<Owner> {
    let bytes = std::fs::read(tree.admin.join(LOCK)).ok()?;
    let owner: Owner = serde_json::from_slice(&bytes).ok()?;
    owner.alive().then_some(owner)
}
