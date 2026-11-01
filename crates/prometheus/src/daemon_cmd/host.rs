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

use pmt_agent::turn::{Interrupt, Issuer, TurnInput, TurnOptions};
use pmt_config::ProfileScope;
use pmt_core::cancel::CancelToken;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_gateway::{Host, HostFuture, Link};
use pmt_rpc::contracts::{
    CompactParams, Compacted, ContextUsage, ForkParams, ResolveKind, ResolveParams,
    SessionCreateParams, SessionList, SessionListParams, SessionRef, SessionSummary, SubmitParams,
    Submitted, Transcript, TranscriptMessage, TurnEnded, TurnError, TurnFinished,
};
use pmt_store::{NewSession, Source, Store, Visibility};

use super::turn::{ToClients, approvals, clarify};
use crate::chat::resume;
use crate::chat::session::model::RouteFlags;
use crate::chat::session::{Factory, Live, Options};

/// A loaded session.
struct Loaded {
    live: Live,
    busy: AtomicBool,
    /// Stops the running turn (`session.interrupt`).
    interrupt: Mutex<Option<Interrupt>>,
}

/// How a client asked for a session to run (`session.create`), kept until
/// the session loads.
#[derive(Debug, Clone, Default)]
struct Setup {
    model: Option<String>,
    provider: Option<String>,
}

impl Setup {
    fn flags(&self) -> RouteFlags<'_> {
        RouteFlags {
            provider: self.provider.as_deref(),
            model: self.model.as_deref(),
            mock_script: None,
        }
    }
}

/// One served profile.
struct Profile {
    scope: ProfileScope,
    store: Arc<Store>,
    /// Session machinery per working directory.
    factories: Mutex<HashMap<PathBuf, Arc<Factory>>>,
    setups: Mutex<HashMap<SessionId, Setup>>,
    loaded: tokio::sync::Mutex<HashMap<SessionId, Arc<Loaded>>>,
}

/// The daemon's host over every served profile.
pub(super) struct DaemonHost {
    profiles: BTreeMap<String, Profile>,
    launch: LaunchEnv,
    turns: Arc<AtomicUsize>,
}

fn session_id(text: &str) -> Result<SessionId, Error> {
    text.parse::<SessionId>()
}

fn busy(id: SessionId) -> Error {
    Error::new(
        &codes::SESSION_BUSY,
        format!("session {id} is running a turn"),
    )
}

impl DaemonHost {
    /// Opens every profile's store.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub(super) fn new(scopes: Vec<ProfileScope>, launch: LaunchEnv) -> Result<Self, Error> {
        let mut profiles = BTreeMap::new();
        for scope in scopes {
            let store = Arc::new(Store::open(&scope)?);
            profiles.insert(
                scope.name().to_string(),
                Profile {
                    scope,
                    store,
                    factories: Mutex::default(),
                    setups: Mutex::default(),
                    loaded: tokio::sync::Mutex::default(),
                },
            );
        }
        Ok(Self {
            profiles,
            launch,
            turns: Arc::default(),
        })
    }

    /// The served profile names.
    pub(super) fn names(&self) -> Vec<String> {
        self.profiles.keys().cloned().collect()
    }

    fn profile(&self, name: &str) -> Result<&Profile, Error> {
        self.profiles.get(name).ok_or_else(|| {
            Error::new(
                &codes::PROFILE_NOT_FOUND,
                format!("profile '{name}' is not served"),
            )
        })
    }

    /// The profile's session machinery for `cwd`.
    fn factory(&self, profile: &Profile, cwd: PathBuf) -> Result<Arc<Factory>, Error> {
        let mut factories = profile.factories.lock().expect("factories poisoned");
        if let Some(factory) = factories.get(&cwd) {
            return Ok(Arc::clone(factory));
        }
        let options = Options {
            interactive: true,
            cwd: cwd.clone(),
            ..Options::default()
        };
        let factory = Arc::new(Factory::new(
            profile.scope.clone(),
            &self.launch,
            &RouteFlags::default(),
            None,
            options,
        )?);
        factories.insert(cwd, Arc::clone(&factory));
        Ok(factory)
    }

    /// Where a session works: its recorded directory, else the profile's
    /// workspace.
    fn cwd_of(profile: &Profile, recorded: Option<&str>) -> PathBuf {
        recorded
            .map(PathBuf::from)
            .filter(|dir| dir.is_dir())
            .unwrap_or_else(|| profile.scope.dir().join("workspace"))
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
        let max = profile.scope.config().sessions.max_resume_messages;
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
            .unwrap_or_default();
        let factory = self.factory(profile, cwd)?;
        let target = factory.models.primary(&setup.flags()).await?;
        let clients = Arc::new(ToClients { link: link.clone() });
        let mut live = factory.open_with(id, &target, Some(clients)).await?;
        let timeout = Duration::from_secs(profile.scope.config().approvals.timeout_secs);
        if let Some(prompts) = live.approvals.take() {
            tokio::spawn(approvals(prompts, link.clone(), timeout));
        }
        if let Some(prompts) = live.clarify.take() {
            tokio::spawn(clarify(prompts, link.clone()));
        }
        // Tool progress is rendered by clients from `session.event`.
        drop(live.progress.take());
        let session = Arc::new(Loaded {
            live,
            busy: AtomicBool::new(false),
            interrupt: Mutex::default(),
        });
        loaded.insert(id, Arc::clone(&session));
        Ok(session)
    }

    /// Refuses a turn when a budget of the session is already spent
    /// (spec: workspace-architecture, JSON-RPC framing and error codes,
    /// Budget refusal over RPC).
    fn check_budget(profile: &Profile, id: SessionId) -> Result<(), Error> {
        let config = profile.scope.config();
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

    async fn submit_turn(
        &self,
        profile: &Profile,
        params: SubmitParams,
        link: Link,
    ) -> Result<Submitted, Error> {
        let id = session_id(&params.session_id)?;
        Self::check_budget(profile, id)?;
        let session = self.load(profile, id, &link).await?;
        if session.busy.swap(true, Ordering::AcqRel) {
            return Err(busy(id));
        }
        let turn_id = pmt_agent::turn::TurnId::new().to_string();
        let scope = profile.scope.clone();
        let turns = Arc::clone(&self.turns);
        turns.fetch_add(1, Ordering::AcqRel);
        let finished_id = turn_id.clone();
        let options = TurnOptions::new(&CancelToken::new());
        *session.interrupt.lock().expect("interrupt poisoned") = Some(options.interrupt.clone());
        tokio::spawn(async move {
            let outcome = session
                .live
                .agent
                .run_turn(Some(&scope), id, TurnInput::text(params.text), options)
                .await;
            link.emit::<TurnEnded>(&finished(finished_id, outcome));
            session.interrupt.lock().expect("interrupt poisoned").take();
            session.busy.store(false, Ordering::Release);
            turns.fetch_sub(1, Ordering::AcqRel);
        });
        Ok(Submitted { turn_id })
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
        },
        Err(err) => TurnFinished {
            turn_id,
            exit_reason: "error".to_owned(),
            final_response: None,
            error: Some(TurnError::of(&err)),
            usage: None,
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
            let profile = self.profile(profile)?;
            let cwd = Self::cwd_of(profile, params.cwd.as_deref());
            std::fs::create_dir_all(&cwd).map_err(|e| Error::io(&codes::HOME_IO, &cwd, &e))?;
            let setup = Setup {
                model: params.model.clone(),
                provider: params.provider.clone(),
            };
            let factory = self.factory(profile, cwd.clone())?;
            let target = factory.models.primary(&setup.flags()).await?;
            let mut new = NewSession::new(Source::Cli);
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
            let profile = self.profile(profile)?;
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

    fn resume<'a>(&'a self, profile: &'a str, link: Link) -> HostFuture<'a, ()> {
        Box::pin(async move {
            let profile = self.profile(profile)?;
            let id = session_id(link.session_id())?;
            self.load(profile, id, &link).await.map(|_| ())
        })
    }

    fn transcript<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
    ) -> HostFuture<'a, Transcript> {
        Box::pin(async move {
            let profile = self.profile(profile)?;
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
            let profile = self.profile(profile)?;
            let id = self::session_id(session_id)?;
            let chars: usize = profile
                .store
                .messages(id, Visibility::Active)?
                .iter()
                .filter_map(|m| m.content.as_ref().map(String::len))
                .sum();
            // The same rough estimate the budget engine uses: 4 bytes a token.
            Ok(ContextUsage {
                tokens: u64::try_from(chars.div_ceil(4)).unwrap_or(u64::MAX),
                context_window: None,
            })
        })
    }

    fn compact<'a>(&'a self, profile: &'a str, params: CompactParams) -> HostFuture<'a, Compacted> {
        Box::pin(async move {
            let profile = self.profile(profile)?;
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
            let outcome = session.live.agent.compress(&profile.scope, &manual).await?;
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
        link: Link,
    ) -> HostFuture<'a, Submitted> {
        Box::pin(async move {
            let profile = self.profile(profile)?;
            self.submit_turn(profile, params, link).await
        })
    }

    fn interrupt<'a>(&'a self, profile: &'a str, session_id: &'a str) -> HostFuture<'a, ()> {
        Box::pin(async move {
            let profile = self.profile(profile)?;
            let id = self::session_id(session_id)?;
            let loaded = profile.loaded.lock().await.get(&id).cloned();
            let running =
                loaded.and_then(|l| l.interrupt.lock().expect("interrupt poisoned").clone());
            if let Some(interrupt) = running {
                interrupt.stop(Issuer::User);
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
            let profile = self.profile(profile)?;
            let request = match params.kind {
                ResolveKind::Resume => resume::Request::Resume(&params.reference),
                ResolveKind::Continue => resume::Request::Continue(&params.reference),
            };
            let cwd = params
                .cwd
                .map_or_else(|| profile.scope.dir().join("workspace"), PathBuf::from);
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
            let profile = self.profile(profile)?;
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

    fn load(&self) -> (usize, usize) {
        let loaded = self
            .profiles
            .values()
            .map(|p| p.loaded.try_lock().map_or(0, |l| l.len()))
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
        })
    }
}
