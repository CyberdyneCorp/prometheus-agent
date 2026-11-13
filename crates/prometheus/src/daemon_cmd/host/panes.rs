//! The desktop panes' daemon side (`pmt_gateway::panes::PanesHost`): the
//! review pane and the Projects sidebar, and the management panes, which
//! reach the same catalogs and changes as the dashboard's pages (so a
//! change made from the desktop runs the code its CLI command runs) and
//! the same pairing flows as its Channels page.

use pmt_config::ProfileScope;
use pmt_gateway::panes::{GitChange, PairingOp, PanesHost};
use pmt_gateway::{HostFuture, Link};
use pmt_rpc::contracts as c;
use serde_json::Value;

use super::DaemonHost;

impl PanesHost for DaemonHost {
    fn scope(&self, profile: &str) -> Option<ProfileScope> {
        self.profile(profile).ok().map(|p| p.scope())
    }

    fn git_review<'a>(
        &'a self,
        profile: &'a str,
        params: c::GitReviewParams,
        link: Link,
    ) -> HostFuture<'a, c::GitReview> {
        Box::pin(async move {
            self.git_review_of(&self.profile(profile)?, params, &link)
                .await
        })
    }

    fn git_change<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
        change: GitChange,
        link: Link,
    ) -> HostFuture<'a, c::GitRan> {
        Box::pin(async move {
            self.git_change_of(&self.profile(profile)?, session_id, change, &link)
                .await
        })
    }

    fn git_message<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
        link: Link,
    ) -> HostFuture<'a, c::GitMessage> {
        Box::pin(async move {
            self.git_message_of(&self.profile(profile)?, session_id, &link)
                .await
        })
    }

    fn projects<'a>(&'a self, profile: &'a str, desktop: bool) -> HostFuture<'a, c::ProjectsList> {
        Box::pin(async move {
            let served = self.profile(profile)?;
            let home = self.launch.user_home().cloned();
            super::projects::list(profile, &served, desktop, home)
        })
    }

    fn project_create<'a>(
        &'a self,
        profile: &'a str,
        path: &'a str,
        name: Option<&'a str>,
    ) -> HostFuture<'a, c::ProjectRow> {
        Box::pin(async move { super::projects::create(&*self.profile(profile)?, path, name) })
    }

    fn manage_catalog<'a>(
        &'a self,
        profile: &'a str,
        catalog: &'a str,
        target: Option<&'a str>,
    ) -> HostFuture<'a, Value> {
        Box::pin(async move {
            let served = self.profile(profile)?;
            match catalog {
                "cron_runs" => cron_runs(&served.store, target.unwrap_or_default()),
                "skills" => Ok(skills(
                    &served.scope(),
                    self.launch.user_home().map(std::path::PathBuf::as_path),
                )),
                "optional_skills" => Ok(optional_skills(
                    &served.scope(),
                    self.launch.user_home().map(std::path::PathBuf::as_path),
                )),
                _ => manage::catalog(self, profile, catalog).await,
            }
        })
    }

    fn manage_change<'a>(
        &'a self,
        profile: &'a str,
        params: c::ManageChangeParams,
    ) -> HostFuture<'a, Value> {
        Box::pin(async move { manage::change(self, profile, params).await })
    }

    fn pairing<'a>(
        &'a self,
        profile: &'a str,
        platform: &'a str,
        op: PairingOp,
    ) -> HostFuture<'a, Value> {
        Box::pin(async move { manage::pairing(self, profile, platform, op) })
    }

    fn running<'a>(&'a self, profile: &'a str) -> HostFuture<'a, Vec<c::CenterSession>> {
        Box::pin(async move {
            let served = self.profile(profile)?;
            let busy: Vec<_> = served
                .loaded
                .lock()
                .await
                .iter()
                .filter(|(_, l)| l.busy.load(std::sync::atomic::Ordering::Acquire))
                .map(|(id, _)| *id)
                .collect();
            Ok(busy
                .into_iter()
                .map(|id| c::CenterSession {
                    profile: profile.to_owned(),
                    session_id: id.to_string(),
                    title: served.store.session(id).ok().and_then(|s| s.title),
                })
                .collect())
        })
    }
}

/// The newest runs of job `job` (the jobs pane's history).
fn cron_runs(store: &pmt_store::Store, job: &str) -> Result<Value, pmt_core::error::Error> {
    let runs: Vec<Value> = store
        .cron_runs(job, None, 20)?
        .iter()
        .map(|r| {
            serde_json::json!({"id": r.id, "status": r.status, "trigger": r.trigger,
                               "scheduled_at": r.scheduled_at, "error": r.error})
        })
        .collect();
    Ok(serde_json::json!({"job": job, "runs": runs}))
}

/// The profile's skills, each with whether it is enabled.
fn skills(scope: &ProfileScope, user_home: Option<&std::path::Path>) -> Value {
    let catalog = pmt_skills::eject::profile_catalog(
        scope,
        None,
        user_home,
        pmt_skills::bundled::Bundled::builtin(),
    );
    let disabled = &scope.config().skills.disabled;
    let rows: Vec<Value> = catalog
        .skills
        .iter()
        .map(|s| serde_json::json!({"name": s.name, "description": s.description, "enabled": !disabled.contains(&s.name)}))
        .collect();
    serde_json::json!({"skills": rows})
}

/// The official optional catalog, each with whether the profile has it.
fn optional_skills(scope: &ProfileScope, user_home: Option<&std::path::Path>) -> Value {
    let installed = pmt_skills::eject::profile_catalog(
        scope,
        None,
        user_home,
        pmt_skills::bundled::Bundled::builtin(),
    );
    let rows: Vec<Value> = pmt_skills::official::skills(&pmt_skills::bundled::Bundled::official())
        .into_iter()
        .map(|(name, path)| {
            serde_json::json!({"name": name, "id": format!("{}{path}", pmt_skills::official::OFFICIAL_SOURCE),
                               "installed": installed.skills.iter().any(|s| s.name == name)})
        })
        .collect();
    serde_json::json!({"skills": rows})
}

/// The management panes over the dashboard's daemon side.
#[cfg(feature = "dashboard")]
mod manage {
    use std::sync::{Arc, LazyLock};

    use pmt_core::error::{Error, codes};
    use pmt_gateway::panes::PairingOp;
    use pmt_rpc::contracts as c;
    use pmt_web::pairing::PairingFlows;
    use pmt_web::{Catalog, Change, DashboardHost as _};
    use serde_json::{Value, json};

    use super::DaemonHost;

    fn unsupported(what: &str) -> Error {
        Error::new(&codes::CLI_USAGE, format!("unknown {what}"))
    }

    /// The desktop's pairing flows (the dashboard keeps its own).
    static FLOWS: LazyLock<Arc<PairingFlows>> = LazyLock::new(Arc::default);

    const CATALOGS: &[(&str, Catalog)] = &[
        ("toolsets", Catalog::Toolsets),
        ("models", Catalog::Models),
        ("plugins", Catalog::Plugins),
        ("mcp", Catalog::Mcp),
        ("cron", Catalog::Cron),
        ("memory", Catalog::Memory),
    ];

    const CHANGES: &[(&str, Change)] = &[
        ("skill", Change::Skill),
        ("skill_archive", Change::SkillArchive),
        ("toolset", Change::Toolset),
        ("cron_update", Change::CronUpdate),
        ("cron_delete", Change::CronDelete),
        ("cron_run", Change::CronRun),
        ("memory", Change::Memory),
    ];

    pub(super) async fn catalog(
        host: &DaemonHost,
        profile: &str,
        name: &str,
    ) -> Result<Value, Error> {
        let what = CATALOGS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, c)| *c)
            .ok_or_else(|| unsupported(&format!("catalog '{name}'")))?;
        host.catalog(profile, what).await
    }

    pub(super) async fn change(
        host: &DaemonHost,
        profile: &str,
        params: c::ManageChangeParams,
    ) -> Result<Value, Error> {
        let change = CHANGES
            .iter()
            .find(|(n, _)| *n == params.change)
            .map(|(_, c)| *c)
            .ok_or_else(|| unsupported(&format!("change '{}'", params.change)))?;
        host.change(profile, change, params.target.as_deref(), params.body)
            .await
    }

    pub(super) fn pairing(
        host: &DaemonHost,
        profile: &str,
        platform: &str,
        op: PairingOp,
    ) -> Result<Value, Error> {
        let flow = pmt_config::platforms::first_party(platform)
            .and_then(|p| p.pairing)
            .ok_or_else(|| {
                Error::new(
                    &codes::CLI_USAGE,
                    format!("the {platform} adapter declares no pairing flow"),
                )
            })?;
        let key = (profile.to_owned(), platform.to_owned());
        match op {
            PairingOp::State => Ok(FLOWS.state(&key)),
            PairingOp::Cancel => {
                FLOWS.cancel(&key);
                Ok(json!({"state": "cancelled"}))
            }
            PairingOp::Start => {
                host.start_pairing(Arc::clone(&FLOWS), profile, flow)?;
                Ok(json!({"state": "starting", "flow": flow.as_str()}))
            }
        }
    }
}

/// Without the dashboard the management panes are not served.
#[cfg(not(feature = "dashboard"))]
mod manage {
    use pmt_core::error::{Error, codes};
    use pmt_gateway::panes::PairingOp;
    use pmt_rpc::contracts as c;
    use serde_json::Value;

    use super::DaemonHost;

    fn off() -> Error {
        Error::new(
            &codes::RPC_NOT_SERVED,
            "this build has no management panes (the dashboard feature is off)",
        )
    }

    pub(super) async fn catalog(_: &DaemonHost, _: &str, _: &str) -> Result<Value, Error> {
        Err(off())
    }

    pub(super) async fn change(
        _: &DaemonHost,
        _: &str,
        _: c::ManageChangeParams,
    ) -> Result<Value, Error> {
        Err(off())
    }

    pub(super) fn pairing(_: &DaemonHost, _: &str, _: &str, _: PairingOp) -> Result<Value, Error> {
        Err(off())
    }
}
