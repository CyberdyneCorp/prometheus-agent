//! The desktop's workspace and management panes (spec: desktop-app, Git
//! review pane; Management panes; Messaging pane shows saved versus
//! running state; Telegram quick setup by QR; Repository discovery for
//! the Projects sidebar): `git.*` on a session's repository, `manage.*`
//! and `adapters.*` on a named profile the connection's grant covers,
//! `projects.*`, and `center.list` across the granted profiles.
//!
//! Writes on a session's repository need a driver attachment; every
//! profile-addressed call names its profile, so a pane showing another
//! profile's job or adapter always targets that profile.

use std::collections::BTreeMap;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_rpc::contracts as c;
use pmt_rpc::{Catalog, RpcError};
use serde_json::Value;

use crate::host::{HostFuture, Link};
use crate::methods::{Need, attached_profile, profile_for, rpc};
use crate::router::Router;
use crate::server::Call;

/// A change of a session's repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitChange {
    /// Stage paths.
    Stage(Vec<String>),
    /// Unstage paths.
    Unstage(Vec<String>),
    /// Discard paths' changes.
    Revert(Vec<String>),
    /// Commit what is staged (and push through the terminal backend).
    Commit {
        /// The message.
        message: String,
        /// Commit & Push.
        push: bool,
    },
    /// Create or switch a branch through the terminal backend.
    Branch {
        /// The branch.
        name: String,
        /// Create it.
        create: bool,
    },
    /// `gh pr create --fill` through the terminal backend.
    PullRequest,
}

/// A pairing flow call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingOp {
    /// Start it.
    Start,
    /// Its current step.
    State,
    /// Cancel it.
    Cancel,
}

fn not_served(what: &str) -> Error {
    Error::new(&codes::RPC_NOT_SERVED, format!("this host has no {what}"))
}

/// What the panes need from the daemon beyond the messaging gateway.
pub trait PanesHost: Send + Sync {
    /// The served profile's configuration now.
    fn scope(&self, profile: &str) -> Option<ProfileScope>;

    /// The review of the session's repository.
    fn git_review<'a>(
        &'a self,
        profile: &'a str,
        params: c::GitReviewParams,
        link: Link,
    ) -> HostFuture<'a, c::GitReview>;

    /// Changes the session's repository.
    fn git_change<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
        change: GitChange,
        link: Link,
    ) -> HostFuture<'a, c::GitRan>;

    /// A commit message for the staged diff (auxiliary model).
    fn git_message<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
        link: Link,
    ) -> HostFuture<'a, c::GitMessage>;

    /// The Projects sidebar's rows; `desktop` asks for discovery by the
    /// desktop's default.
    fn projects<'a>(&'a self, profile: &'a str, desktop: bool) -> HostFuture<'a, c::ProjectsList>;

    /// Creates a project of one repository.
    fn project_create<'a>(
        &'a self,
        profile: &'a str,
        path: &'a str,
        name: Option<&'a str>,
    ) -> HostFuture<'a, c::ProjectRow>;

    /// One catalog of a profile (`target`: the job of `cron_runs`).
    fn manage_catalog<'a>(
        &'a self,
        profile: &'a str,
        catalog: &'a str,
        target: Option<&'a str>,
    ) -> HostFuture<'a, Value>;

    /// One change of a profile.
    fn manage_change<'a>(
        &'a self,
        profile: &'a str,
        params: c::ManageChangeParams,
    ) -> HostFuture<'a, Value>;

    /// A pairing flow of a profile's adapter.
    fn pairing<'a>(
        &'a self,
        profile: &'a str,
        platform: &'a str,
        op: PairingOp,
    ) -> HostFuture<'a, Value>;

    /// The profile's sessions with a running turn.
    fn running<'a>(&'a self, profile: &'a str) -> HostFuture<'a, Vec<c::CenterSession>>;
}

fn panes(call: &Call) -> Result<&dyn PanesHost, RpcError> {
    call.server
        .host()
        .panes()
        .ok_or_else(|| rpc(&not_served("desktop panes")))
}

/// Registers the panes' methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router
        .register::<c::GitReviewCall, _, _>(catalog, git_review)?
        .register::<c::GitStage, _, _>(catalog, |call, p: c::GitPaths| {
            git_ack(call, p.session_id, GitChange::Stage(p.paths))
        })?
        .register::<c::GitUnstage, _, _>(catalog, |call, p: c::GitPaths| {
            git_ack(call, p.session_id, GitChange::Unstage(p.paths))
        })?
        .register::<c::GitRevert, _, _>(catalog, |call, p: c::GitPaths| {
            git_ack(call, p.session_id, GitChange::Revert(p.paths))
        })?
        .register::<c::GitCommit, _, _>(catalog, |call, p: c::GitCommitParams| {
            git_change(
                call,
                p.session_id,
                GitChange::Commit {
                    message: p.message,
                    push: p.push,
                },
            )
        })?
        .register::<c::GitBranch, _, _>(catalog, |call, p: c::GitBranchParams| {
            git_change(
                call,
                p.session_id,
                GitChange::Branch {
                    name: p.name,
                    create: p.create,
                },
            )
        })?
        .register::<c::GitPr, _, _>(catalog, |call, p: c::SessionRef| {
            git_change(call, p.session_id, GitChange::PullRequest)
        })?
        .register::<c::GitMessageCall, _, _>(catalog, git_message)?
        .register::<c::ProjectsListCall, _, _>(catalog, projects_list)?
        .register::<c::ProjectsCreate, _, _>(catalog, projects_create)?
        .register::<c::ManageCatalog, _, _>(catalog, manage_catalog)?
        .register::<c::ManageChange, _, _>(catalog, manage_change)?
        .register::<c::AdaptersList, _, _>(catalog, adapters_list)?
        .register::<c::AdaptersSave, _, _>(catalog, adapters_save)?
        .register::<c::AdaptersRestart, _, _>(catalog, adapters_restart)?
        .register::<c::AdaptersPairingStart, _, _>(catalog, |call, p: c::AdapterRef| {
            pairing(call, p, PairingOp::Start)
        })?
        .register::<c::AdaptersPairingState, _, _>(catalog, |call, p: c::AdapterRef| {
            pairing(call, p, PairingOp::State)
        })?
        .register::<c::AdaptersPairingCancel, _, _>(catalog, |call, p: c::AdapterRef| async move {
            pairing(call, p, PairingOp::Cancel)
                .await
                .map(|_| c::Ack::YES)
        })?
        .register::<c::CenterListCall, _, _>(catalog, center)?;
    Ok(())
}

async fn git_review(call: Call, params: c::GitReviewParams) -> Result<c::GitReview, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Attached)?;
    let link = call.server.link(&params.session_id);
    panes(&call)?
        .git_review(&profile, params, link)
        .await
        .map_err(|e| rpc(&e))
}

async fn git_change(
    call: Call,
    session_id: String,
    change: GitChange,
) -> Result<c::GitRan, RpcError> {
    let profile = attached_profile(&call, &session_id, Need::Driver)?;
    let link = call.server.link(&session_id);
    panes(&call)?
        .git_change(&profile, &session_id, change, link)
        .await
        .map_err(|e| rpc(&e))
}

async fn git_ack(call: Call, session_id: String, change: GitChange) -> Result<c::Ack, RpcError> {
    git_change(call, session_id, change)
        .await
        .map(|_| c::Ack::YES)
}

async fn git_message(call: Call, params: c::SessionRef) -> Result<c::GitMessage, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Driver)?;
    let link = call.server.link(&params.session_id);
    panes(&call)?
        .git_message(&profile, &params.session_id, link)
        .await
        .map_err(|e| rpc(&e))
}

fn desktop(call: &Call) -> bool {
    call.conn.client() == Some(c::ClientKind::Desktop)
}

async fn projects_list(
    call: Call,
    params: c::ProjectsListParams,
) -> Result<c::ProjectsList, RpcError> {
    let profile = profile_for(&call, params.profile.as_deref())?;
    panes(&call)?
        .projects(&profile, desktop(&call))
        .await
        .map_err(|e| rpc(&e))
}

async fn projects_create(
    call: Call,
    params: c::ProjectsCreateParams,
) -> Result<c::ProjectRow, RpcError> {
    let profile = profile_for(&call, params.profile.as_deref())?;
    panes(&call)?
        .project_create(&profile, &params.path, params.name.as_deref())
        .await
        .map_err(|e| rpc(&e))
}

async fn manage_catalog(call: Call, params: c::ManageCatalogParams) -> Result<Value, RpcError> {
    let profile = profile_for(&call, Some(&params.profile))?;
    panes(&call)?
        .manage_catalog(&profile, &params.catalog, params.target.as_deref())
        .await
        .map_err(|e| rpc(&e))
}

async fn manage_change(call: Call, params: c::ManageChangeParams) -> Result<Value, RpcError> {
    profile_for(&call, Some(&params.profile))?;
    let profile = params.profile.clone();
    panes(&call)?
        .manage_change(&profile, params)
        .await
        .map_err(|e| rpc(&e))
}

fn scope_of(call: &Call, profile: &str) -> Result<ProfileScope, RpcError> {
    panes(call)?.scope(profile).ok_or_else(|| {
        rpc(&Error::new(
            &codes::PROFILE_NOT_FOUND,
            format!("profile '{profile}' is not served"),
        ))
    })
}

async fn adapters_list(call: Call, params: c::ProfileParams) -> Result<c::AdapterList, RpcError> {
    let profile = profile_for(&call, Some(&params.profile))?;
    let scope = scope_of(&call, &profile)?;
    Ok(c::AdapterList {
        adapters: crate::messaging::pane::rows(&scope, call.server.messaging()),
    })
}

fn row(call: &Call, profile: &str, platform: &str) -> Result<c::AdapterRow, RpcError> {
    let scope = scope_of(call, profile)?;
    crate::messaging::pane::rows(&scope, call.server.messaging())
        .into_iter()
        .find(|r| r.platform == platform)
        .ok_or_else(|| {
            rpc(&Error::new(
                &codes::CLI_USAGE,
                format!("no adapter {platform}"),
            ))
        })
}

async fn adapters_save(
    call: Call,
    params: c::AdapterSaveParams,
) -> Result<c::AdapterRow, RpcError> {
    use crate::messaging::pane::{Save, SaveError, save};
    let profile = profile_for(&call, Some(&params.profile))?;
    row(&call, &profile, &params.platform)?;
    let scope = scope_of(&call, &profile)?;
    let secrets: BTreeMap<&str, &str> = params
        .secrets
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let request = Save {
        enabled: params.enabled,
        options: Vec::new(),
        secrets: secrets.into_iter().collect(),
        clear: params.clear.iter().map(String::as_str).collect(),
    };
    save(&scope, &params.platform, &request).map_err(|e| match e {
        SaveError::UnknownSecret(name) => rpc(&Error::new(
            &codes::CLI_USAGE,
            format!("{name} is not a secret of the {} adapter", params.platform),
        )),
        SaveError::Invalid(err) | SaveError::Refused(err) => rpc(&err),
    })?;
    // The daemon reads the saved file now, so the row shows what was saved.
    call.server
        .host()
        .config_reload(&profile)
        .await
        .map_err(|e| rpc(&e))?;
    row(&call, &profile, &params.platform)
}

async fn adapters_restart(call: Call, params: c::AdapterRef) -> Result<c::AdapterRow, RpcError> {
    let profile = profile_for(&call, Some(&params.profile))?;
    let gateway = call
        .server
        .messaging()
        .ok_or_else(|| rpc(&not_served("messaging gateway running")))?;
    gateway
        .restart_platform(&profile, &params.platform)
        .await
        .map_err(|e| rpc(&e))?;
    row(&call, &profile, &params.platform)
}

async fn pairing(call: Call, params: c::AdapterRef, op: PairingOp) -> Result<Value, RpcError> {
    let profile = profile_for(&call, Some(&params.profile))?;
    panes(&call)?
        .pairing(&profile, &params.platform, op)
        .await
        .map_err(|e| rpc(&e))
}

async fn center(call: Call, _params: c::CenterParams) -> Result<c::CenterList, RpcError> {
    let host = panes(&call)?;
    let mut list = c::CenterList::default();
    for profile in &call.conn.grant.profiles {
        list.sessions
            .extend(host.running(profile).await.map_err(|e| rpc(&e))?);
    }
    let profile_of: BTreeMap<String, String> = list
        .sessions
        .iter()
        .map(|s| (s.session_id.clone(), s.profile.clone()))
        .collect();
    for (request_id, session_id, method, params) in call.server.hub().open_requests() {
        let Some(profile) = profile_of.get(&session_id) else {
            continue;
        };
        if method != "approval.request" {
            continue;
        }
        let summary = params["subject"]
            .as_str()
            .or_else(|| params["tool"].as_str())
            .unwrap_or_default()
            .to_owned();
        list.approvals.push(c::CenterApproval {
            profile: profile.clone(),
            session_id,
            request_id,
            summary,
        });
    }
    for session in &list.sessions {
        let children = call
            .server
            .host()
            .subagents(&session.profile, &session.session_id)
            .await
            .unwrap_or_default();
        list.subagents.extend(
            children
                .subagents
                .into_iter()
                .filter(|s| s.status == "running" || s.status == "queued")
                .map(|s| c::CenterSubagent {
                    profile: session.profile.clone(),
                    session_id: session.session_id.clone(),
                    goal: s.title.unwrap_or_default(),
                    subagent_id: s.subagent_id,
                }),
        );
    }
    Ok(list)
}
