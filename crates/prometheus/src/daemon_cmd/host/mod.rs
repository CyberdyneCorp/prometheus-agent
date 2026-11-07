//! The daemon's [`Host`]: every served profile's store and sessions, run
//! with the same session machinery as an in-process chat (spec:
//! workspace-architecture, Clients attach to a running daemon). A session
//! is loaded on its first `session.resume` or `prompt.submit` and stays
//! loaded; its events and prompts reach the attached clients through its
//! [`Link`].

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_agent::turn::{Interrupt, Issuer};
use pmt_config::{ProfileScope, SharedScope};
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_gateway::{Author, Host, HostFuture, Link};
use pmt_rpc::contracts::{
    BannerInfo, ClientKind, CommandCatalog, CompactParams, Compacted, CompleteParams, Completions,
    ConfigValue, ContextUsage, DispatchParams, Dispatched, ForkParams, ModelOptions, ResolveKind,
    ResolveParams, SessionCreateParams, SessionList, SessionListParams, SessionRef, SessionSetup,
    SessionSummary, SubmitParams, Submitted, Transcript, TranscriptMessage, TurnError,
    TurnFinished,
};
use pmt_store::{NewSession, Source, Store, Visibility};
use tracing::Instrument as _;

use super::turn::{SecretsToClients, SudoToClients, ToClients, approvals, clarify};

mod commands;
mod cron;
mod cron_admit;
mod cron_continue;
mod cron_fire;
mod cron_send;
mod delegations;
mod discovery;
mod gateway;
mod notify;
mod reload;
mod send_tool;
mod setup;
mod turns;
mod worktrees;

use crate::chat::resume;
use crate::chat::session::{Clients, Factory, Live};
pub(super) use cron::schedule as schedule_cron;
pub(super) use reload::watch;
pub(super) use worktrees::sweep_worktrees;

/// A loaded session.
struct Loaded {
    live: Live,
    /// The machinery it was opened with (its commands run on it).
    factory: Arc<Factory>,
    /// How its client asked it to run (re-applied to each reload).
    setup: SessionSetup,
    busy: AtomicBool,
    /// Wakes those waiting for the running turn to end.
    idle: tokio::sync::Notify,
    /// Stops the running turn (`session.interrupt`).
    interrupt: Mutex<Option<Interrupt>>,
    /// Inputs admitted while a turn ran, first in first out; `busy` is
    /// only cleared under this lock once it is empty.
    queue: Mutex<std::collections::VecDeque<turns::Queued>>,
    /// The running turn's id (a steered input joins it).
    current: Mutex<Option<String>>,
    /// A one-shot run's session: it ends once its turn is done, as an
    /// in-process one-shot run's does.
    oneshot: bool,
}

impl Loaded {
    /// Waits until no turn runs.
    async fn until_idle(&self) {
        loop {
            let ended = self.idle.notified();
            if !self.busy.load(Ordering::Acquire) {
                return;
            }
            ended.await;
        }
    }

    /// Stops the running turn, if any.
    fn stop(&self) {
        let running = self.interrupt.lock().expect("interrupt poisoned").clone();
        if let Some(interrupt) = running {
            interrupt.stop(Issuer::User);
        }
    }
}

/// One served profile.
struct Profile {
    /// The configuration new sessions start with, replaced by a reload
    /// (shared with every factory, whose shared tools read it).
    current: SharedScope,
    store: Arc<Store>,
    /// What the reloads did, for `daemon status`.
    config: Mutex<reload::State>,
    /// Session machinery per working directory and setup.
    factories: Mutex<HashMap<(PathBuf, SessionSetup), Arc<Factory>>>,
    setups: Mutex<HashMap<SessionId, SessionSetup>>,
    loaded: tokio::sync::Mutex<HashMap<SessionId, Arc<Loaded>>>,
    /// Wakes the profile's scheduler loop (a webhook queued a run).
    cron_wake: tokio::sync::Notify,
}

/// The daemon's host over every served profile.
pub(super) struct DaemonHost {
    /// Shared with the tasks that run turns (a one-shot session is closed
    /// by its task).
    /// Grows when `gateway reload` serves a profile created since start.
    profiles: std::sync::RwLock<BTreeMap<String, Arc<Profile>>>,
    launch: LaunchEnv,
    /// The daemon's `--set` overrides, kept for reloads.
    set: Vec<String>,
    /// The profile that sees the launch environment's secrets.
    launch_profile: String,
    turns: Arc<AtomicUsize>,
    /// `gateway.limits.max_concurrent_sessions`: turns beyond it wait.
    slots: pmt_gateway::slots::Slots,
    /// The messaging gateway, for the `send_message` tool.
    gateway: send_tool::GatewayCell,
}

fn session_id(text: &str) -> Result<SessionId, Error> {
    text.parse::<SessionId>()
}

/// Work on `profile`'s session: its records (and those of the tasks it
/// spawns) go to that profile's `logs/` as the `agent` component, not to
/// the daemon's `gateway.log` (spec: observability, Structured logs).
fn session_span(profile: &str, session_id: &str) -> tracing::Span {
    tracing::info_span!("session", component = "agent", profile, session_id)
}

fn busy(id: SessionId) -> Error {
    Error::new(
        &codes::SESSION_BUSY,
        format!("session {id} is running a turn"),
    )
}

impl Profile {
    /// Opens the profile's store (and reaps its orphaned containers).
    fn open(scope: ProfileScope) -> Result<Arc<Self>, Error> {
        let store = Arc::new(Store::open(&scope)?);
        crate::chat::session::backend::reap_orphans_in_background(&scope);
        // Children a previous daemon left running end `interrupted`; their
        // completions are owed to their sessions.
        pmt_delegation::host::deliver::recover(&store);
        let state = reload::State::new(&scope);
        Ok(Arc::new(Self {
            current: SharedScope::new(scope),
            config: Mutex::new(state),
            store,
            factories: Mutex::default(),
            setups: Mutex::default(),
            loaded: tokio::sync::Mutex::default(),
            cron_wake: tokio::sync::Notify::new(),
        }))
    }

    /// The configuration new sessions start with.
    fn scope(&self) -> ProfileScope {
        self.current.get()
    }
}

impl DaemonHost {
    /// Opens every profile's store.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub(super) fn new(
        scopes: Vec<ProfileScope>,
        launch: LaunchEnv,
        set: Vec<String>,
        launch_profile: String,
    ) -> Result<Self, Error> {
        let mut profiles = BTreeMap::new();
        for scope in scopes {
            profiles.insert(scope.name().to_string(), Profile::open(scope)?);
        }
        Ok(Self {
            profiles: std::sync::RwLock::new(profiles),
            launch,
            set,
            launch_profile,
            turns: Arc::default(),
            slots: pmt_gateway::slots::Slots::new(0),
            gateway: Arc::default(),
        })
    }

    /// Bounds the turns running at once (`gateway.limits.max_concurrent_sessions`).
    pub(super) fn with_turn_cap(mut self, max: u32) -> Self {
        self.slots = pmt_gateway::slots::Slots::new(max);
        self
    }

    /// Hands sessions the started messaging gateway (`send_message`).
    pub(super) fn set_gateway(&self, gateway: pmt_gateway::messaging::Gateway) {
        let _ = self.gateway.set(gateway);
        // Plugins post, inject and act through the same gateway.
        crate::plugins_host::messaging::set_gateway(Arc::clone(&self.gateway));
    }

    /// The served profile names.
    pub(super) fn names(&self) -> Vec<String> {
        self.served().keys().cloned().collect()
    }

    /// The served profiles now.
    fn served(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, Arc<Profile>>> {
        self.profiles.read().expect("profiles poisoned")
    }

    /// Every served profile, by name (a snapshot).
    fn all_profiles(&self) -> Vec<(String, Arc<Profile>)> {
        self.served()
            .iter()
            .map(|(n, p)| (n.clone(), Arc::clone(p)))
            .collect()
    }

    fn profile(&self, name: &str) -> Result<Arc<Profile>, Error> {
        self.served().get(name).cloned().ok_or_else(|| {
            Error::new(
                &codes::PROFILE_NOT_FOUND,
                format!("profile '{name}' is not served"),
            )
        })
    }

    /// Where a session works: its recorded directory, else the profile's
    /// workspace.
    fn cwd_of(profile: &Profile, recorded: Option<&str>) -> PathBuf {
        recorded
            .map(PathBuf::from)
            .filter(|dir| dir.is_dir())
            .unwrap_or_else(|| profile.scope().dir().join("workspace"))
    }

    /// The loaded session `id`, loading it (with `link`) when needed.
    async fn load(
        &self,
        profile: &Profile,
        id: SessionId,
        link: &Link,
    ) -> Result<Arc<Loaded>, Error> {
        let mut loaded = profile.loaded.lock().await;
        if let Some(session) = loaded.get(&id) {
            return Ok(Arc::clone(session));
        }
        let row = profile.store.session(id)?;
        let max = profile.scope().config().sessions.max_resume_messages;
        profile.store.check_resumable(id, max)?;
        if row.ended_at.is_some() {
            profile.store.reopen_session(id)?;
        }
        let cwd = Self::cwd_of(profile, row.cwd.as_deref());
        std::fs::create_dir_all(&cwd).map_err(|e| Error::io(&codes::HOME_IO, &cwd, &e))?;
        let setup = profile
            .setups
            .lock()
            .expect("setups poisoned")
            .remove(&id)
            .or_else(|| frozen_policy(profile, &row))
            .unwrap_or_default();
        let factory = self.factory(profile, cwd, &setup)?;
        let target = factory.models.primary(&setup::flags(&setup)).await?;
        // A chat's session tells the chat when its background processes
        // end; an interactive client session tells its clients. Other
        // sessions (one-shot runs, cron, subagents) cannot receive a later
        // notice, so quiet-wait never promises one there.
        let oneshot = row.source_tag.as_deref() == Some(pmt_store::sessions::ONESHOT_TAG);
        let completions = match row.origin {
            pmt_store::Origin::Gateway { .. } => Some(notify::completions(
                Arc::clone(&self.gateway),
                profile.scope().name().to_string(),
                id.to_string(),
            )),
            pmt_store::Origin::None if !oneshot => Some(notify::to_clients(link.clone())),
            _ => None,
        };
        // Only a person at a client of an interactive session is asked
        // for a sudo password.
        let sudo = matches!(row.origin, pmt_store::Origin::None if !oneshot).then(|| {
            Arc::new(SudoToClients {
                link: link.clone(),
                runtime: tokio::runtime::Handle::current(),
            }) as Arc<dyn pmt_tools::terminal::sudo::SudoPrompt>
        });
        let (inbox, delegation) = self.delegation_inbox(profile, &row, id, link, oneshot)?;
        let clients = Clients {
            delegation: delegation.clone(),
            events: Arc::new(ToClients { link: link.clone() }),
            secrets: Arc::new(SecretsToClients {
                link: link.clone(),
                runtime: tokio::runtime::Handle::current(),
            }),
            completions,
            sudo,
        };
        let mut live = factory.open_with(id, &target, Some(clients)).await?;
        if let Some(effective) = self.effective(profile, &setup, &live.scope) {
            live.config.replace(effective);
        }
        let timeout = Duration::from_secs(profile.scope().config().approvals.timeout_secs);
        if let Some(prompts) = live.approvals.take() {
            tokio::spawn(approvals(prompts, link.clone(), timeout).in_current_span());
        }
        if let Some(prompts) = live.clarify.take() {
            tokio::spawn(clarify(prompts, link.clone()).in_current_span());
        }
        // Tool progress is rendered by clients from `session.event`.
        drop(live.progress.take());
        let session = Arc::new(Loaded {
            live,
            factory,
            setup,
            busy: AtomicBool::new(false),
            idle: tokio::sync::Notify::new(),
            interrupt: Mutex::default(),
            queue: Mutex::default(),
            current: Mutex::default(),
            oneshot,
        });
        loaded.insert(id, Arc::clone(&session));
        if let Some(inbox) = &inbox {
            inbox.bind(&session);
        }
        if let Some(delegation) = delegation {
            delegations::deliver_owed(profile, &session, delegation);
        }
        Ok(session)
    }

    /// Where a loaded session's background delegations deliver: a client
    /// session gets a queued turn, a gateway chat an injected one; other
    /// sessions (one-shot runs, cron, subagents) wait for their children.
    #[allow(clippy::type_complexity)] // the inbox and its trait object
    fn delegation_inbox(
        &self,
        profile: &Profile,
        row: &pmt_store::Session,
        id: SessionId,
        link: &Link,
        oneshot: bool,
    ) -> Result<
        (
            Option<Arc<delegations::ClientInbox>>,
            Option<Arc<dyn pmt_delegation::host::Completions>>,
        ),
        Error,
    > {
        let name = profile.scope().name().to_string();
        Ok(match row.origin {
            pmt_store::Origin::Gateway { .. } => (
                None,
                Some(Arc::new(delegations::ChatInbox {
                    gateway: Arc::clone(&self.gateway),
                    profile: name,
                    session: id.to_string(),
                }) as _),
            ),
            pmt_store::Origin::None if !oneshot => {
                let inbox =
                    delegations::ClientInbox::new(self, &self.profile(&name)?, id, link.clone());
                (Some(Arc::clone(&inbox)), Some(inbox as _))
            }
            _ => (None, None),
        })
    }

    /// Refuses a turn when a budget of the session is already spent
    /// (spec: workspace-architecture, JSON-RPC framing and error codes,
    /// Budget refusal over RPC).
    fn check_budget(profile: &Profile, id: SessionId) -> Result<(), Error> {
        let scope = profile.scope();
        let config = scope.config();
        let zone = pmt_obs::time::profile_zone(&config.cron)?;
        let place = pmt_obs::budget::Place::of(&profile.store, id, None)?;
        let spent = pmt_obs::budget::Budgets::new(zone).status(
            &profile.store,
            &config.budgets,
            &place,
            false,
            jiff::Timestamp::now(),
        )?;
        match spent.into_iter().find(|(limit, used)| *used >= limit.limit) {
            Some((limit, used)) => Err(Error::new(
                &codes::BUDGET_EXCEEDED,
                format!(
                    "the {} {} budget is exhausted ({} of {})",
                    limit.scope.as_str(),
                    limit.unit.as_str(),
                    pmt_obs::budget::format_amount(limit.unit, used),
                    pmt_obs::budget::format_amount(limit.unit, limit.limit)
                ),
            )
            .with_hint("raise it with 'prometheus config set budgets.<key>' or /budget set")),
            None => Ok(()),
        }
    }
}

/// The stored source of a session a client of kind `client` creates.
fn source_of(client: Option<ClientKind>) -> Source {
    match client {
        Some(ClientKind::Tui) => Source::Tui,
        Some(ClientKind::Desktop) => Source::Desktop,
        Some(ClientKind::Dashboard) => Source::Dashboard,
        Some(ClientKind::Acp) => Source::Acp,
        Some(ClientKind::Eval) => Source::Eval,
        Some(ClientKind::Cli) | None => Source::Cli,
    }
}

fn finished(turn_id: String, outcome: Result<pmt_agent::turn::TurnResult, Error>) -> TurnFinished {
    match outcome {
        Ok(result) => TurnFinished {
            turn_id,
            exit_reason: result.exit_reason.as_str().to_owned(),
            final_response: result.final_response,
            error: result.error.as_ref().map(TurnError::of),
            usage: serde_json::to_value(result.usage).ok(),
            report: None,
        },
        Err(err) => TurnFinished {
            turn_id,
            exit_reason: "error".to_owned(),
            final_response: None,
            error: Some(TurnError::of(&err)),
            usage: None,
            report: None,
        },
    }
}

impl Host for DaemonHost {
    fn owns<'a>(&'a self, profile: &'a str, session_id: &'a str) -> HostFuture<'a, bool> {
        Box::pin(async move {
            let (Ok(profile), Ok(id)) = (self.profile(profile), self::session_id(session_id))
            else {
                return Ok(false);
            };
            match profile.store.session(id) {
                Ok(_) => Ok(true),
                Err(err) if err.kind() == pmt_core::error::ErrorKind::NotFound => Ok(false),
                Err(err) => Err(err),
            }
        })
    }

    fn create<'a>(
        &'a self,
        profile: &'a str,
        params: SessionCreateParams,
    ) -> HostFuture<'a, SessionRef> {
        Box::pin(async move {
            let profile = &self.profile(profile)?;
            let cwd = Self::cwd_of(profile, params.cwd.as_deref());
            std::fs::create_dir_all(&cwd).map_err(|e| Error::io(&codes::HOME_IO, &cwd, &e))?;
            let setup = params.setup.clone();
            let factory = self.factory(profile, cwd.clone(), &setup)?;
            let target = factory.models.primary(&setup::flags(&setup)).await?;
            let mut new = NewSession::new(source_of(params.source));
            new.provider = Some(target.provider.clone());
            new.model.clone_from(&target.model);
            if let Some(tag) = &params.source_tag {
                pmt_store::sessions::check_source_tag(tag)?;
                new.source_tag = Some(tag.clone());
            }
            new.cwd = Some(cwd.to_string_lossy().into_owned());
            let session = profile.store.create_session(&new)?;
            profile
                .setups
                .lock()
                .expect("setups poisoned")
                .insert(session.id, setup);
            Ok(SessionRef {
                session_id: session.id.to_string(),
            })
        })
    }

    fn list<'a>(
        &'a self,
        profile: &'a str,
        params: SessionListParams,
    ) -> HostFuture<'a, SessionList> {
        Box::pin(async move {
            let profile = &self.profile(profile)?;
            let options = pmt_store::ListOptions {
                limit: Some(params.limit.unwrap_or(50)),
                ..pmt_store::ListOptions::default()
            };
            let loaded = profile.loaded.lock().await;
            let sessions = profile
                .store
                .list_sessions(&options)?
                .into_iter()
                .map(|s| SessionSummary {
                    live: loaded
                        .get(&s.id)
                        .is_some_and(|l| l.busy.load(Ordering::Acquire)),
                    session_id: s.id.to_string(),
                    title: s.title,
                    started_at: s.started_at.to_string(),
                })
                .collect();
            Ok(SessionList { sessions })
        })
    }

    fn resume<'a>(
        &'a self,
        profile: &'a str,
        setup: Option<SessionSetup>,
        link: Link,
    ) -> HostFuture<'a, ()> {
        let span = session_span(profile, link.session_id());
        Box::pin(
            async move {
                let profile = &self.profile(profile)?;
                let id = session_id(link.session_id())?;
                if let Some(setup) = setup {
                    Self::set_up(profile, id, setup).await?;
                }
                self.load(profile, id, &link).await.map(|_| ())
            }
            .instrument(span),
        )
    }

    fn dispatch<'a>(
        &'a self,
        profile: &'a str,
        params: DispatchParams,
        link: Link,
    ) -> HostFuture<'a, Dispatched> {
        let span = session_span(profile, &params.session_id);
        Box::pin(
            async move {
                let profile = &self.profile(profile)?;
                self.dispatch_command(profile, params, link).await
            }
            .instrument(span),
        )
    }

    fn transcript<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
    ) -> HostFuture<'a, Transcript> {
        Box::pin(async move {
            let profile = &self.profile(profile)?;
            let id = self::session_id(session_id)?;
            let messages = profile
                .store
                .messages(id, Visibility::Active)?
                .into_iter()
                .map(|m| TranscriptMessage {
                    id: m.id.to_string(),
                    role: m.role.as_str().to_owned(),
                    text: m.content,
                })
                .collect();
            Ok(Transcript { messages })
        })
    }

    fn context_usage<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
    ) -> HostFuture<'a, ContextUsage> {
        Box::pin(async move {
            let profile = &self.profile(profile)?;
            let id = self::session_id(session_id)?;
            let chars: usize = profile
                .store
                .messages(id, Visibility::Active)?
                .iter()
                .filter_map(|m| m.content.as_ref().map(String::len))
                .sum();
            let loaded = profile.loaded.lock().await.get(&id).cloned();
            let context_window = match loaded {
                Some(session) => {
                    use pmt_agent::turn::model::ModelPort as _;
                    session.live.model.route().await.context_window
                }
                None => None,
            };
            let cost = profile
                .store
                .usage_totals(pmt_store::UsageScope::Session(id))?;
            // The same rough estimate the budget engine uses: 4 bytes a token.
            Ok(ContextUsage {
                tokens: u64::try_from(chars.div_ceil(4)).unwrap_or(u64::MAX),
                context_window,
                cost_usd: (cost.calls > cost.unpriced_calls).then_some(cost.cost_usd),
            })
        })
    }

    fn egress<'a>(
        &'a self,
        profile: &'a str,
        params: pmt_rpc::contracts::EgressSetParams,
        link: Link,
    ) -> HostFuture<'a, pmt_rpc::contracts::EgressState> {
        Box::pin(async move {
            let profile = &self.profile(profile)?;
            let id = session_id(&params.session_id)?;
            let session = self.load(profile, id, &link).await?;
            let hosts = &session.live.egress.hosts;
            crate::egress::apply_set(hosts, params.mode.as_deref(), &params.allow, &params.deny)?;
            let (mode, allow, deny) = crate::egress::snapshot(hosts);
            Ok(pmt_rpc::contracts::EgressState { mode, allow, deny })
        })
    }

    fn worktree<'a>(
        &'a self,
        profile: &'a str,
        request: pmt_gateway::worktree::WorktreeCall,
        link: Link,
    ) -> HostFuture<'a, pmt_gateway::worktree::WorktreeReply> {
        Box::pin(self.worktree_call(profile, request, link))
    }

    fn files_undo<'a>(
        &'a self,
        profile: &'a str,
        params: pmt_rpc::contracts::FilesUndoParams,
        link: Link,
    ) -> HostFuture<'a, pmt_rpc::contracts::FilesUndone> {
        Box::pin(async move {
            let profile = &self.profile(profile)?;
            let id = session_id(&params.session_id)?;
            let session = self.load(profile, id, &link).await?;
            let report = crate::chat::commands::files::undo_files(
                &profile.store,
                &session.live,
                params.turns.unwrap_or(1),
                params.force,
                params.dry_run,
            )?;
            let shown = |paths: Vec<std::path::PathBuf>| {
                paths.iter().map(|p| p.display().to_string()).collect()
            };
            Ok(pmt_rpc::contracts::FilesUndone {
                turns: report.turns,
                restored: shown(report.restored),
                removed: shown(report.removed),
                conflicts: report.conflicts,
            })
        })
    }

    fn files_rollback<'a>(
        &'a self,
        profile: &'a str,
        params: pmt_rpc::contracts::FilesRollbackParams,
        link: Link,
    ) -> HostFuture<'a, pmt_rpc::contracts::FilesRolledBack> {
        Box::pin(async move {
            let profile = &self.profile(profile)?;
            let id = session_id(&params.session_id)?;
            let session = self.load(profile, id, &link).await?;
            let output = crate::chat::commands::files::rollback_live(
                &profile.store,
                &session.live,
                params.checkpoint.map(|n| n as usize),
                params.file.as_deref(),
                params.diff,
                params.all,
            )?;
            Ok(pmt_rpc::contracts::FilesRolledBack { output })
        })
    }

    fn lsp_status(&self, profile: &str) -> Result<pmt_rpc::contracts::LspStatus, Error> {
        let scope = self.profile(profile)?.scope();
        Ok(crate::lsp_cmd::status(&scope))
    }

    fn cron_blueprints(&self, profile: &str) -> Result<pmt_rpc::contracts::BlueprintList, Error> {
        let scope = self.profile(profile)?.scope();
        Ok(crate::cron_host::blueprint_rpc::list(&scope, &self.launch))
    }

    fn cron_blueprint_fill(
        &self,
        profile: &str,
        params: &pmt_rpc::contracts::BlueprintFillParams,
    ) -> Result<pmt_rpc::contracts::BlueprintFilled, Error> {
        let served = self.profile(profile)?;
        let store = Arc::clone(&served.store);
        crate::cron_host::blueprint_rpc::fill(&served.scope(), &self.launch, store, params)
    }

    fn vault(&self, profile: &str) -> Result<pmt_gateway::vault::VaultAccess, Error> {
        let scope = self.profile(profile)?.scope();
        Ok(pmt_gateway::vault::VaultAccess {
            vault: crate::vault_cmd::open(&scope),
            scope,
        })
    }

    fn compact<'a>(&'a self, profile: &'a str, params: CompactParams) -> HostFuture<'a, Compacted> {
        Box::pin(async move {
            let profile = &self.profile(profile)?;
            let id = session_id(&params.session_id)?;
            let loaded = profile.loaded.lock().await.get(&id).cloned();
            let Some(session) = loaded else {
                return Err(Error::new(
                    &codes::SESSION_BUSY,
                    format!("session {id} is not loaded; resume it first"),
                ));
            };
            if session.busy.load(Ordering::Acquire) {
                return Err(busy(id));
            }
            let manual = pmt_agent::compress::manual::Manual {
                focus: params.focus,
                ..pmt_agent::compress::manual::Manual::default()
            };
            let outcome = session
                .live
                .agent
                .compress(&session.live.config.get(), &manual)
                .await?;
            Ok(Compacted {
                status: format!("{:?}", outcome.status).to_lowercase(),
                tokens_before: outcome.tokens_before,
                tokens_after: outcome.tokens_after,
            })
        })
    }

    fn submit<'a>(
        &'a self,
        profile: &'a str,
        params: SubmitParams,
        author: Author,
        link: Link,
    ) -> HostFuture<'a, Submitted> {
        let span = session_span(profile, &params.session_id);
        Box::pin(
            async move {
                let profile = &self.profile(profile)?;
                self.submit_turn(profile, params, (author, link), None)
                    .await
            }
            .instrument(span),
        )
    }

    fn commands<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
        client: ClientKind,
    ) -> HostFuture<'a, CommandCatalog> {
        Box::pin(async move {
            let profile = &self.profile(profile)?;
            self.commands_of(profile, session_id, client).await
        })
    }

    fn complete_path<'a>(
        &'a self,
        _profile: &'a str,
        params: CompleteParams,
    ) -> HostFuture<'a, Completions> {
        Box::pin(async move { Ok(Self::paths_of(&params.session_id, &params.prefix)) })
    }

    fn model_options<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
    ) -> HostFuture<'a, ModelOptions> {
        Box::pin(async move {
            let profile = &self.profile(profile)?;
            self.models_of(profile, session_id).await
        })
    }

    fn banner<'a>(&'a self, profile: &'a str, session_id: &'a str) -> HostFuture<'a, BannerInfo> {
        Box::pin(async move { Self::banner_of(&*self.profile(profile)?, session_id).await })
    }

    fn reload_plugins<'a>(
        &'a self,
        profile: &'a str,
        plugin: Option<&'a str>,
    ) -> HostFuture<'a, String> {
        Box::pin(async move {
            let profile = self.profile(profile)?;
            // `plugins enable` and friends write `config.toml` and ask for
            // the reload at once: read it now rather than at the next poll.
            self.check(&profile).await;
            let scope = profile.scope();
            let cwd = std::env::current_dir().unwrap_or_else(|_| scope.dir().to_path_buf());
            let loaded = crate::plugins_host::reload(&scope, &cwd).current();
            crate::plugins_host::reload::check_target(&loaded, plugin)?;
            Ok(crate::plugins_host::reload::report(&loaded, plugin, false))
        })
    }

    fn config_get<'a>(&'a self, profile: &'a str, key: &'a str) -> HostFuture<'a, ConfigValue> {
        Box::pin(async move { Self::config_value(&*self.profile(profile)?, key) })
    }

    fn unload_idle<'a>(&'a self, profile: &'a str, session_id: &'a str) -> HostFuture<'a, bool> {
        Box::pin(async move {
            let profile = &self.profile(profile)?;
            Ok(Self::unload(profile, self::session_id(session_id)?).await)
        })
    }

    fn close_idle<'a>(&'a self, profile: &'a str, session_id: &'a str) -> HostFuture<'a, bool> {
        Box::pin(
            async move {
                let profile = &self.profile(profile)?;
                Ok(Self::close_unused(profile, self::session_id(session_id)?).await)
            }
            .instrument(session_span(profile, session_id)),
        )
    }

    fn interrupt<'a>(&'a self, profile: &'a str, session_id: &'a str) -> HostFuture<'a, ()> {
        Box::pin(async move {
            let profile = &self.profile(profile)?;
            let id = self::session_id(session_id)?;
            if let Some(session) = profile.loaded.lock().await.get(&id).cloned() {
                session.stop();
            }
            Ok(())
        })
    }

    fn resolve<'a>(
        &'a self,
        profile: &'a str,
        params: ResolveParams,
    ) -> HostFuture<'a, SessionRef> {
        Box::pin(async move {
            let profile = &self.profile(profile)?;
            let request = match params.kind {
                ResolveKind::Resume => resume::Request::Resume(&params.reference),
                ResolveKind::Continue => resume::Request::Continue(&params.reference),
            };
            let cwd = params
                .cwd
                .map_or_else(|| profile.scope().dir().join("workspace"), PathBuf::from);
            let place = resume::Place {
                cwd: &cwd,
                terminal: params.terminal.as_deref(),
            };
            let found = resume::resolve(&profile.store, &request, place)?.ok_or_else(|| {
                Error::new(
                    &codes::SESSION_NOT_FOUND,
                    "no session matches the reference",
                )
            })?;
            Ok(SessionRef {
                session_id: found.to_string(),
            })
        })
    }

    fn fork<'a>(&'a self, profile: &'a str, params: ForkParams) -> HostFuture<'a, SessionRef> {
        Box::pin(async move {
            let profile = &self.profile(profile)?;
            let id = session_id(&params.session_id)?;
            let at = params
                .at_message_id
                .as_deref()
                .map(str::parse::<pmt_core::id::MessageId>)
                .transpose()?;
            let fork = profile
                .store
                .fork_session(id, at, params.title.as_deref())?;
            Ok(SessionRef {
                session_id: fork.id.to_string(),
            })
        })
    }

    fn config(&self) -> Vec<pmt_rpc::control::ProfileConfig> {
        self.config_states()
    }

    fn load(&self) -> (usize, usize) {
        let loaded = self
            .all_profiles()
            .iter()
            .map(|(_, p)| p.loaded.try_lock().map_or(0, |l| l.len()))
            .sum();
        (loaded, self.turns.load(Ordering::Acquire))
    }

    fn drain(
        &self,
        deadline: Duration,
    ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let until = tokio::time::Instant::now() + deadline;
            while self.turns.load(Ordering::Acquire) > 0 && tokio::time::Instant::now() < until {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            for (name, profile) in self.all_profiles() {
                Self::close_all(&name, &profile).await;
            }
        })
    }
}

/// The setup of the chat policy frozen with a gateway session, which keeps
/// shaping it after a daemon restart (spec: messaging-gateway, Per-chat
/// policies).
fn frozen_policy(profile: &Profile, row: &pmt_store::Session) -> Option<SessionSetup> {
    let pmt_store::Origin::Gateway { platform, .. } = &row.origin else {
        return None;
    };
    let text = profile.store.session_policy(row.id).ok().flatten()?;
    let policy: pmt_config::schema::ChatPolicy = serde_json::from_str(&text).ok()?;
    Some(pmt_gateway::messaging::policy::session_setup(
        &policy, platform,
    ))
}
