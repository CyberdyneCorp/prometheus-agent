//! Creating a tree (spec: git-worktrees, Worktree creation): fetch the
//! default remote, branch `prometheus/<name>` off the remote tip of the
//! current branch's upstream (or the default branch), check it out under
//! `.worktrees/<name>` with hooks, fsmonitor and filters off, and mark it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use pmt_core::error::{Error, codes};

use crate::names::{self, Kind};
use crate::repo::Repo;
use crate::tree::{self, Meta, Tree};

/// How long the pre-creation fetch may take before the local tip is used.
const FETCH_TIMEOUT: Duration = Duration::from_mins(1);

/// What to create.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    /// The tree's name; `None` generates `prom-<8 hex>`.
    pub name: Option<&'a str>,
    /// Who named it (forced to `Generated` for a generated name).
    pub kind: Kind,
    /// The branch prefix (`prometheus`).
    pub branch_prefix: &'a str,
    /// `worktrees.sync_remote`: fetch first.
    pub sync_remote: bool,
    /// Branch from this commit instead of the remote tip (a subagent's
    /// tree starts at its parent's `HEAD`, so its branch merges back).
    pub base: Option<&'a str>,
}

impl Default for Request<'_> {
    fn default() -> Self {
        Self {
            name: None,
            kind: Kind::Named,
            branch_prefix: "prometheus",
            sync_remote: true,
            base: None,
        }
    }
}

/// A created tree.
#[derive(Debug, Clone)]
pub struct Created {
    /// The repository.
    pub repo: Repo,
    /// The tree.
    pub tree: Tree,
    /// The ref it was branched from (`origin/main`, `HEAD`...).
    pub base: String,
    /// What the user should know (a failed fetch).
    pub notices: Vec<String>,
}

/// Creates a tree of the repository containing `dir`.
///
/// # Errors
///
/// `worktree.not_a_repository`, `worktree.invalid_name` (malformed, or
/// `worktree '<name>' already exists`) and `worktree.git`.
pub fn create(dir: &Path, request: &Request<'_>) -> Result<Created, Error> {
    let repo = Repo::discover(dir)?;
    let (name, kind) = match request.name {
        Some(name) => {
            names::validate(name)?;
            (name.to_owned(), request.kind)
        }
        None => (names::generated(), Kind::Generated),
    };
    let git = repo.git()?;
    let path = repo.trees_dir().join(&name);
    let branch = names::branch(request.branch_prefix, &name);
    if path.exists()
        || tree::find(&repo, &name).is_some()
        || git.test(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
    {
        return Err(Error::new(
            &codes::WORKTREE_INVALID_NAME,
            format!("worktree '{name}' already exists"),
        ));
    }
    repo.exclude_trees()?;
    let mut notices = Vec::new();
    let base = match request.base {
        Some(base) => base.to_owned(),
        None => base_ref(&repo, request.sync_remote, &mut notices)?,
    };
    std::fs::create_dir_all(repo.trees_dir()).map_err(|e| {
        crate::git::git_error(&["worktree"], &format!("cannot create .worktrees: {e}"))
    })?;
    let path_text = path.to_string_lossy().into_owned();
    git.run(&[
        "worktree",
        "add",
        "--no-track",
        "-b",
        &branch,
        &path_text,
        &base,
    ])?;
    let path = path.canonicalize().unwrap_or(path);
    let admin = tree::admin_of(&path).ok_or_else(|| {
        crate::git::git_error(&["worktree"], "the new tree has no admin directory")
    })?;
    let meta = Meta {
        name,
        kind,
        branch,
        created: tree::now_secs(),
        last_decision: None,
    };
    tree::write_meta(&admin, &meta)
        .map_err(|e| crate::git::git_error(&["worktree"], &format!("cannot mark the tree: {e}")))?;
    Ok(Created {
        repo,
        tree: Tree { path, admin, meta },
        base,
        notices,
    })
}

/// The commit a new tree starts from: after a fetch, the upstream of the
/// current branch or the remote's default branch; without a remote, or
/// when the fetch fails (with a notice), the local `HEAD`.
fn base_ref(repo: &Repo, sync: bool, notices: &mut Vec<String>) -> Result<String, Error> {
    let git = repo.git()?;
    if !git.test(&["rev-parse", "--verify", "--quiet", "HEAD"]) {
        return Err(Error::new(
            &codes::WORKTREE_NOT_A_REPOSITORY,
            format!("{} has no commit to branch from", repo.root.display()),
        ));
    }
    let Some(remote) = repo.default_remote(&git) else {
        return Ok("HEAD".to_owned());
    };
    if sync {
        let fetched =
            git.with_timeout(FETCH_TIMEOUT)
                .output(&["fetch", "--no-tags", "--quiet", &remote]);
        let failure = match fetched {
            Ok(out) if out.success => None,
            Ok(out) => Some(out.stderr),
            Err(err) => Some(err.message().to_owned()),
        };
        if let Some(why) = failure {
            notices.push(format!(
                "could not fetch {remote} ({}); the worktree starts from the local tip",
                why.lines().next().unwrap_or("unknown error")
            ));
            return Ok("HEAD".to_owned());
        }
    }
    if let Ok(upstream) = git.run(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
        && git.test(&["rev-parse", "--verify", "--quiet", &upstream])
    {
        return Ok(upstream);
    }
    let default = format!("refs/remotes/{remote}/HEAD");
    if let Ok(target) = git.run(&["symbolic-ref", "--short", &default]) {
        return Ok(target);
    }
    Ok("HEAD".to_owned())
}

/// The directory a tree named `name` of `repo` lives in.
#[must_use]
pub fn path_of(repo: &Repo, name: &str) -> PathBuf {
    repo.trees_dir().join(name)
}
