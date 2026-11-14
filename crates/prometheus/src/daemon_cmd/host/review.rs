//! The git review pane's daemon side (spec: desktop-app, Git review
//! pane): reviews and index changes of the session's repository on this
//! machine, Commit & Push, branches and pull requests through the
//! session's own terminal backend, and commit messages from the
//! `commit_message` auxiliary route.
//!
//! `last-turn` needs the working tree as it was when the most recent turn
//! in the repository began: each turn start snapshots the tree of its
//! session's repository (a tree object, nothing else changes) and the
//! newest snapshot per repository is kept for the daemon's lifetime.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use pmt_core::error::{Error, codes};
use pmt_gateway::Link;
use pmt_gateway::panes::GitChange;
use pmt_providers::auxiliary::AuxTask;
use pmt_rpc::contracts::{GitFile, GitMessage, GitRan, GitReview, GitReviewParams};
use pmt_tools::environments::Environments;
use pmt_tools::terminal::TerminalArgs;
use pmt_worktrees::review::{Scope, Tree};

use super::{DaemonHost, Profile, session_id};

/// The snapshot of each repository's working tree when its most recent
/// turn began, by the tree's top folder.
static TURN_STARTS: LazyLock<Mutex<HashMap<PathBuf, String>>> = LazyLock::new(Mutex::default);

/// How long a turn start waits for its snapshot (a huge repository is
/// left without a `last-turn` scope rather than delaying the turn).
const SNAPSHOT_BOUND: Duration = Duration::from_secs(20);

/// What a commit message is asked for with.
const COMMIT_PROMPT: &str = "Write a concise git commit message for the staged diff below: \
    a summary line of at most 72 characters, then, only if needed, a blank line and a short \
    body. Answer with the message only.";

fn starts() -> std::sync::MutexGuard<'static, HashMap<PathBuf, String>> {
    TURN_STARTS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The working tree of session `id`'s workspace on this machine, if it is
/// inside a git repository.
fn tree_of_session(id: &str) -> Option<Tree> {
    let files = Environments::process().session(id)?.files.clone()?;
    files
        .on_host()
        .then(|| Tree::at(&files.cwd()).ok())
        .flatten()
}

/// A turn of session `id` begins: snapshots its repository's working tree.
pub(super) async fn turn_started(id: &str) {
    let id = id.to_owned();
    let snap = pmt_core::spawn::blocking(move || {
        let tree = tree_of_session(&id)?;
        let snapshot = tree.snapshot();
        Some((tree.root().to_path_buf(), snapshot))
    });
    match tokio::time::timeout(SNAPSHOT_BOUND, snap).await {
        Ok(Ok(Some((root, Ok(tree))))) => {
            starts().insert(root, tree);
        }
        Ok(Ok(Some((root, Err(err))))) => {
            tracing::warn!(root = %root.display(), "no last-turn snapshot: {err}");
            starts().remove(&root);
        }
        Ok(Ok(None)) => {}
        _ => tracing::warn!("the last-turn snapshot took too long; the scope is unavailable"),
    }
}

fn gh_available() -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let pathext = std::env::var_os("PATHEXT");
    pmt_tools::terminal::shell::find_executable("gh", &path, pathext.as_deref()).is_some()
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, Error> + Send + 'static,
) -> Result<T, Error> {
    pmt_core::spawn::blocking(work)
        .await
        .map_err(|e| Error::new(&codes::WORKTREE_GIT, e.to_string()))?
}

impl DaemonHost {
    /// The session's repository on this machine (loading the session).
    async fn session_tree(
        &self,
        profile: &Arc<Profile>,
        session: &str,
        link: &Link,
    ) -> Result<Tree, Error> {
        let files = self.session_files(profile, session, link).await?;
        if !files.on_host() {
            return Err(Error::new(
                &codes::CAPABILITY_NOT_GRANTED,
                "the review pane needs a workspace on this machine; this session's backend keeps it elsewhere",
            ));
        }
        let cwd = files.cwd();
        blocking(move || Tree::at(&cwd)).await
    }

    pub(super) async fn git_review_of(
        &self,
        profile: &Arc<Profile>,
        params: GitReviewParams,
        link: &Link,
    ) -> Result<GitReview, Error> {
        let scope = Scope::parse(&params.scope).ok_or_else(|| {
            Error::new(
                &codes::CLI_USAGE,
                format!(
                    "unknown scope '{}': uncommitted, branch or last-turn",
                    params.scope
                ),
            )
        })?;
        let tree = self.session_tree(profile, &params.session_id, link).await?;
        let start = starts().get(tree.root()).cloned();
        let review = blocking(move || tree.review(scope, start.as_deref())).await?;
        Ok(GitReview {
            root: review.root.display().to_string(),
            branch: review.branch,
            ahead: review.ahead,
            behind: review.behind,
            scope: params.scope,
            files: review
                .files
                .into_iter()
                .map(|f| GitFile {
                    path: f.path,
                    status: f.status,
                    staged: f.staged,
                    patch: f.patch,
                })
                .collect(),
            read_only: review.read_only,
            gh: gh_available(),
        })
    }

    pub(super) async fn git_change_of(
        &self,
        profile: &Arc<Profile>,
        session: &str,
        change: GitChange,
        link: &Link,
    ) -> Result<GitRan, Error> {
        let tree = self.session_tree(profile, session, link).await?;
        let root = tree.root().to_path_buf();
        let done = |commit: Option<String>| GitRan {
            commit,
            output: String::new(),
            ok: true,
        };
        match change {
            GitChange::Stage(paths) => blocking(move || tree.stage(&paths))
                .await
                .map(|()| done(None)),
            GitChange::Unstage(paths) => blocking(move || tree.unstage(&paths))
                .await
                .map(|()| done(None)),
            GitChange::Revert(paths) => blocking(move || tree.revert(&paths))
                .await
                .map(|()| done(None)),
            GitChange::Commit {
                message,
                push: false,
            } => blocking(move || tree.commit(&message))
                .await
                .map(|c| done(Some(c))),
            GitChange::Commit {
                message,
                push: true,
            } => {
                let commit = blocking(move || tree.commit(&message)).await?;
                let mut ran = in_terminal(session, &root, "git push").await?;
                ran.commit = Some(commit);
                Ok(ran)
            }
            GitChange::Branch { name, create } => {
                let flag = if create { "-c " } else { "" };
                let command = format!("git switch {flag}{}", pmt_sandbox::ssh::quote(&name));
                in_terminal(session, &root, &command).await
            }
            GitChange::PullRequest => in_terminal(session, &root, "gh pr create --fill").await,
        }
    }

    pub(super) async fn git_message_of(
        &self,
        profile: &Arc<Profile>,
        session: &str,
        link: &Link,
    ) -> Result<GitMessage, Error> {
        let tree = self.session_tree(profile, session, link).await?;
        let diff = blocking(move || tree.staged_diff()).await?;
        if diff.trim().is_empty() {
            return Err(Error::new(&codes::WORKTREE_GIT, "nothing is staged"));
        }
        let loaded = self.load(profile, session_id(session)?, link).await?;
        let message = loaded
            .factory
            .models
            .ask_auxiliary(
                AuxTask::CommitMessage,
                session_id(session)?,
                COMMIT_PROMPT,
                &diff,
                300,
            )
            .await?
            .unwrap_or_default();
        Ok(GitMessage {
            message: message.trim().to_owned(),
        })
    }
}

/// Runs `line` in the repository through the session's terminal backend
/// (its sandbox, container or SSH host, credentials and approvals).
async fn in_terminal(session: &str, root: &Path, line: &str) -> Result<GitRan, Error> {
    let terminal = Environments::process()
        .session(session)
        .and_then(|env| env.terminal.clone())
        .ok_or_else(|| {
            Error::new(
                &codes::CAPABILITY_NOT_GRANTED,
                "this session has no terminal, so git cannot run through it",
            )
        })?;
    let mut args = TerminalArgs::new(line);
    args.workdir = Some(root.display().to_string());
    let result = terminal.run(args).await;
    Ok(GitRan {
        commit: None,
        ok: result.exit_code == Some(0),
        output: result.error.unwrap_or(result.output),
    })
}
