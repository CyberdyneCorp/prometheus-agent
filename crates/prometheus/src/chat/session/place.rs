//! Where a session works: its working directory and the execution
//! environment attached for it (spec: git-worktrees, In-session worktree
//! command).
//!
//! `/worktree new` moves a running session into a new tree without
//! restarting it: the terminal, file tools, snapshots and language-server
//! trust are rebuilt for the tree and re-attached under the same session
//! id, while the agent, its system prompt and its tool array stay exactly
//! as they were, so the provider's prompt cache survives. The move is
//! announced in the command's reply and in the next tool result instead.
//! The session's egress policy and its refusal sink carry over, and its
//! delegated children start in the directory recorded on the session.

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use pmt_core::capability::{Capability, CapabilitySet};
use pmt_core::error::Error;
use pmt_tools::browser::BrowserExec;
use pmt_tools::environments::Attachment;
use pmt_tools::snapshots::SessionSnapshots;
use pmt_tools::snapshots::worktree::Worktree;
use pmt_tools::terminal::Terminal;
use pmt_tools::terminal::sync::SyncBack;
use pmt_worktrees::tree::OwnerLock;

use super::env::{Attached, Denials};
use super::{Factory, Live};

/// What the environment is built for.
pub struct At<'a> {
    /// The working directory.
    pub cwd: &'a Path,
    /// The daemon clients the session's prompts and notices go to.
    pub clients: Option<&'a super::Clients>,
    /// The running session's egress policy (`None`: a new session's).
    pub hosts: Option<Arc<pmt_net::egress::HostPolicy>>,
    /// The running session's refusal sink (`None`: a new one).
    pub denials: Option<Denials>,
}

impl<'a> At<'a> {
    /// A new session in `cwd` with `clients`.
    #[must_use]
    pub fn open(cwd: &'a Path, clients: Option<&'a super::Clients>) -> Self {
        Self {
            cwd,
            clients,
            hosts: None,
            denials: None,
        }
    }
}

/// The cwd-bound parts of a session's environment.
pub(super) struct Environment {
    browser: Option<Arc<BrowserExec>>,
    terminal: Option<Arc<Terminal>>,
    sync: Option<Arc<SyncBack>>,
    snapshots: Option<Arc<SessionSnapshots>>,
    worktree: Worktree,
    denials: Denials,
    /// A moved session's own egress registration and child proxy (the
    /// first environment's stay with the session).
    egress: Option<crate::egress::Running>,
    // Dropped last: detaches the environment from the process table.
    _attachment: Attachment,
}

impl Environment {
    /// The cwd-bound parts of `attached`, its egress and its downgrades.
    pub(super) fn of(
        attached: Attached,
    ) -> (Self, crate::egress::Running, super::downgrades::Downgrades) {
        let environment = Self {
            browser: attached.browser,
            terminal: attached.terminal,
            sync: attached.sync,
            snapshots: attached.snapshots,
            worktree: attached.worktree,
            denials: attached.denials,
            egress: None,
            _attachment: attached.attachment,
        };
        (environment, attached.egress, attached.downgrades)
    }
}

/// A session's working place.
pub struct Place {
    cwd: RwLock<PathBuf>,
    environment: RwLock<Option<Environment>>,
    owner: RwLock<Option<OwnerLock>>,
}

impl Place {
    /// The place of a session opened in `cwd` with `environment`; a
    /// Prometheus tree it works in is marked as owned for the sweep.
    pub(super) fn new(cwd: PathBuf, environment: Environment, session: &str) -> Self {
        Self {
            owner: RwLock::new(owner_of(&cwd, session)),
            cwd: RwLock::new(cwd),
            environment: RwLock::new(Some(environment)),
        }
    }

    /// The working directory.
    #[must_use]
    pub fn cwd(&self) -> PathBuf {
        self.cwd
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn with<T>(&self, pick: impl FnOnce(&Environment) -> T) -> Option<T> {
        self.environment
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(pick)
    }

    /// File snapshots, when on.
    #[must_use]
    pub fn snapshots(&self) -> Option<Arc<SessionSnapshots>> {
        self.with(|e| e.snapshots.clone()).flatten()
    }

    /// Where `/diff` runs git.
    #[must_use]
    pub fn worktree(&self) -> Worktree {
        self.with(|e| e.worktree.clone())
            .unwrap_or_else(|| Worktree::host(&self.cwd()))
    }

    /// The session's `browser_exec`.
    #[must_use]
    pub fn browser(&self) -> Option<Arc<BrowserExec>> {
        self.with(|e| e.browser.clone()).flatten()
    }

    /// Sync-back of a remote workspace.
    #[must_use]
    pub fn sync(&self) -> Option<Arc<SyncBack>> {
        self.with(|e| e.sync.clone()).flatten()
    }

    /// Releases the tree it works in to the sweep (session end).
    pub fn release(&self) {
        self.owner
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }

    fn denials(&self) -> Option<Denials> {
        self.with(|e| Arc::clone(&e.denials))
    }

    fn take(&self) -> Option<Environment> {
        self.environment
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// Marks the Prometheus tree `cwd` lies in as owned by `session`.
fn owner_of(cwd: &Path, session: &str) -> Option<OwnerLock> {
    let (_, tree) = pmt_worktrees::tree::containing(cwd)?;
    OwnerLock::take(&tree, Some(session))
        .inspect_err(|err| tracing::debug!(error = %err, "worktree owner not recorded"))
        .ok()
}

/// `capabilities` plus writes to the shared git directories a commit in
/// a linked worktree needs (spec: git-worktrees, Subagent worktree
/// isolation: a child commits on its branch from its own tree). The main
/// checkout's hooks and configuration stay out of reach.
#[must_use]
pub fn with_worktree_git(capabilities: &CapabilitySet, cwd: &Path) -> CapabilitySet {
    let Some(dirs) = pmt_worktrees::tree::shared_git_dirs(cwd) else {
        return capabilities.clone();
    };
    let grants = dirs
        .writes
        .iter()
        .map(|d| Capability::write_under(d))
        .chain([Capability::read_under(&dirs.common)])
        .collect();
    capabilities.union(&grants)
}

impl Live {
    /// The session's working directory.
    #[must_use]
    pub fn cwd(&self) -> PathBuf {
        self.place.cwd()
    }

    /// Moves the session to `cwd` (a new worktree): rebuilds its terminal,
    /// file tools, snapshots and language-server trust there, ends the
    /// old terminal's background processes, records the new directory on
    /// the session (a resume reopens it) and announces the move in the
    /// next tool result. The agent, prompt and tools are untouched.
    ///
    /// # Errors
    ///
    /// Environment and store errors; the session then has no environment
    /// until it is moved again or reopened.
    pub async fn retarget(&self, factory: &Factory, cwd: &Path) -> Result<(), Error> {
        let session = factory.store.session(self.id)?;
        let target = self.model.target();
        let denials = self.place.denials();
        if let Some(old) = self.place.take() {
            if let Some(browser) = &old.browser {
                browser.close_session(&self.id.to_string()).await;
            }
            if let Some(terminal) = &old.terminal {
                terminal.close().await;
            }
            drop(old);
        }
        let at = At {
            cwd,
            clients: self.clients.as_ref(),
            hosts: Some(Arc::clone(&self.egress.hosts)),
            denials,
        };
        let attached = factory.environment(
            &session,
            &target,
            &self.capabilities,
            &self.config,
            &self.skills,
            &at,
        )?;
        let (mut environment, egress, _) = Environment::of(attached);
        environment.egress = Some(egress);
        *self
            .place
            .cwd
            .write()
            .unwrap_or_else(PoisonError::into_inner) = cwd.to_path_buf();
        *self
            .place
            .environment
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(environment);
        let owner = owner_of(cwd, &self.id.to_string());
        *self
            .place
            .owner
            .write()
            .unwrap_or_else(PoisonError::into_inner) = owner;
        factory
            .store
            .set_session_cwd(self.id, &cwd.to_string_lossy())?;
        self.tools.announce(format!(
            "[the session's working directory is now {}]",
            cwd.display()
        ));
        Ok(())
    }
}
