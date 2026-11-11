//! `worktree.*` for daemon sessions (spec: git-worktrees, In-session
//! worktree command) and the 6-hourly sweep of every served profile's
//! recorded repositories (Repository registry and sweep schedule).

use std::sync::Arc;

use pmt_core::error::Error;
use pmt_gateway::Link;
use pmt_gateway::worktree::{WorktreeCall, WorktreeReply};
use pmt_worktrees::registry;
use pmt_worktrees::sweep::Mode;

use super::DaemonHost;
use crate::chat::commands::worktree;
use crate::chat::session::delegation::isolation::settings_of;

impl DaemonHost {
    /// Runs one `worktree.*` request on its session.
    pub(super) async fn worktree_call(
        &self,
        profile: &str,
        request: WorktreeCall,
        link: Link,
    ) -> Result<WorktreeReply, Error> {
        let profile = &self.profile(profile)?;
        let id = super::session_id(request.session_id())?;
        let session = self.load(profile, id, &link).await?;
        let scope = profile.scope();
        let live = &session.live;
        Ok(match request {
            WorktreeCall::New(params) => WorktreeReply::Created(
                worktree::new(&session.factory, live, &scope, params.name.as_deref()).await?,
            ),
            WorktreeCall::Status(_) => WorktreeReply::Status(worktree::status(live)),
            WorktreeCall::List(_) => WorktreeReply::Listing(worktree::list(live)?),
            WorktreeCall::Prune(params) => {
                WorktreeReply::Pruned(worktree::prune(&scope, live, params.dry_run)?)
            }
        })
    }
}

/// Sweeps every served profile's recorded repositories every 6 hours
/// until the daemon stops (the first sweep one period after start).
pub(in crate::daemon_cmd) async fn sweep_worktrees(host: Arc<DaemonHost>) {
    let mut tick = tokio::time::interval(registry::SWEEP_EVERY);
    tick.tick().await;
    loop {
        tick.tick().await;
        for (name, profile) in host.all_profiles() {
            let scope = profile.scope();
            let settings = settings_of(&scope);
            let dir = scope.dir().to_path_buf();
            let swept = pmt_core::spawn::blocking(move || {
                registry::sweep_all(
                    &dir,
                    &settings,
                    pmt_worktrees::tree::now_secs(),
                    Mode::Apply,
                )
            })
            .await;
            match swept {
                Ok(swept) => {
                    for repo in swept {
                        if let Err(err) = repo.decisions {
                            tracing::warn!(profile = %name, repo = %repo.repo.root.display(), error = %err, "worktree sweep failed");
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(profile = %name, error = %err, "worktree sweep panicked");
                }
            }
        }
    }
}
