//! What the messaging gateway needs from the daemon host (spec:
//! messaging-gateway, Profiles served by one daemon; Agent cache and
//! eviction; Gateway slash commands, quick commands and hooks): each
//! served profile's store and configuration, conversation sessions with a
//! messaging origin, steering, the shared command registry seen from the
//! gateway surface, and unloading an idle agent without ending its
//! session.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use pmt_config::{Home, Launch, Overrides, ProfileScope};
use pmt_core::error::Error;
use pmt_gateway::messaging::{
    Agents, BusyPolicy, Conversation, Lookup, MenuCommand, ModelChoice, UpdateFromChat, UpdateRun,
};
use pmt_gateway::{HostFuture, Link};
use pmt_rpc::contracts::{Dispatched, SessionSetup};
use pmt_store::{EndReason, LineageKind, NewSession, Origin, Source, Store};
use tracing::Instrument as _;

use super::{DaemonHost, Profile, session_id, session_span, setup};
use crate::chat::commands;
use crate::slash::{self, Resolution, Surface};

/// The gateway-surface commands of `scope` (built-ins and quick
/// commands) and its user-invocable skills, for native command menus.
fn menu_commands(
    scope: &ProfileScope,
    store: &Store,
    user_home: Option<&Path>,
) -> Vec<MenuCommand> {
    let registry = commands::profile_registry(scope);
    let builtins = registry
        .builtins()
        .iter()
        .filter(|c| c.on(Surface::Gateway))
        .map(|c| MenuCommand {
            name: c.name.to_owned(),
            description: c.description.to_owned(),
            takes_args: !c.args.is_empty(),
        });
    let externals = registry
        .externals()
        .filter(|e| e.surfaces.contains(&Surface::Gateway))
        .map(|e| MenuCommand {
            name: e.name.clone(),
            description: e.description.clone(),
            takes_args: true,
        });
    let catalog = pmt_skills::eject::profile_catalog(
        scope,
        None,
        user_home,
        pmt_skills::bundled::Bundled::builtin(),
    );
    let disabled = &scope.config().skills.disabled;
    let enabled = catalog
        .skills
        .iter()
        .filter(|s| !disabled.contains(&s.name));
    let table =
        pmt_skills::slash::SlashTable::new(enabled.map(AsRef::as_ref), &registry.builtin_names());
    // Built-ins first, then skills by recent use: a capped menu keeps
    // the ones people reach for.
    let last_used = |name: &str| {
        store
            .skill_usage(name)
            .ok()
            .flatten()
            .and_then(|u| u.last_activity_at)
    };
    let mut skills: Vec<(Option<jiff::Timestamp>, MenuCommand)> = table
        .commands
        .iter()
        .map(|(slug, name)| {
            let command = MenuCommand {
                name: slug.clone(),
                description: catalog
                    .skills
                    .iter()
                    .find(|s| &s.name == name)
                    .map_or_else(|| format!("skill {name}"), |s| s.description.clone()),
                takes_args: true,
            };
            (last_used(name), command)
        })
        .collect();
    skills.sort_by_key(|(used, _)| std::cmp::Reverse(*used));
    builtins
        .chain(externals)
        .chain(skills.into_iter().map(|(_, c)| c))
        .collect()
}

/// A chat policy's `preload_skills` as the session's first user-side
/// context (spec: messaging-gateway, Per-chat policy thread matching and
/// skill preload): one authored user row holding each skill's content,
/// which the first request merges with the first message. Never in the
/// system prompt, so every chat of the profile shares its cached prefix.
fn preload_context(
    scope: &ProfileScope,
    store: &Store,
    user_home: Option<&Path>,
    session: pmt_core::id::SessionId,
    names: &[String],
) -> Result<(), Error> {
    let catalog = pmt_skills::eject::profile_catalog(
        scope,
        None,
        user_home,
        pmt_skills::bundled::Bundled::builtin(),
    );
    let mut text = String::new();
    for name in names {
        let Ok(skill) = catalog.lookup(name) else {
            tracing::warn!(skill = %name, "preloaded skill not found; skipped");
            continue;
        };
        let body =
            pmt_skills::load::substitute(&skill.body, &skill.dir.display(), &session.to_string());
        let _ = std::fmt::Write::write_fmt(
            &mut text,
            format_args!("\n\n## Skill: {}\n\n{}", skill.name, body.trim()),
        );
        let used = store.record_skill_activity(
            &skill.name,
            pmt_store::SkillActivity::Used,
            pmt_store::CreatedBy::User,
        );
        if let Err(err) = used {
            tracing::warn!(skill = %skill.name, "skill usage not recorded: {err}");
        }
    }
    if text.is_empty() {
        return Ok(());
    }
    let row = pmt_agent::turn::transcript::notice_row(
        &format!(
            "# Preloaded skills\n\nThis conversation's skills; follow them where they apply.{text}"
        ),
        "preloaded_skills",
    );
    store.append_message(session, &row)?;
    Ok(())
}

fn busy_policy(policy: slash::BusyPolicy) -> BusyPolicy {
    match policy {
        slash::BusyPolicy::Reject => BusyPolicy::Reject,
        slash::BusyPolicy::Queue => BusyPolicy::Queue,
        slash::BusyPolicy::InterruptThenDispatch => BusyPolicy::InterruptThenDispatch,
        slash::BusyPolicy::Immediate => BusyPolicy::Immediate,
    }
}

/// What a resolution is to the gateway: unknown commands are ordinary text.
fn lookup_of(resolution: Resolution) -> Lookup {
    match resolution {
        Resolution::Builtin { command, args } => Lookup::Command {
            name: command.name.to_owned(),
            args,
            busy: busy_policy(command.busy),
        },
        Resolution::External { command, args } => Lookup::Command {
            busy: busy_policy(command.busy),
            name: command.name,
            args,
        },
        Resolution::Unknown { .. } => Lookup::Text,
        other => Lookup::Reply(other.message().unwrap_or_default()),
    }
}

impl Agents for DaemonHost {
    fn store(&self, profile: &str) -> Option<Arc<Store>> {
        self.profile(profile).ok().map(|p| Arc::clone(&p.store))
    }

    fn scope(&self, profile: &str) -> Option<ProfileScope> {
        self.profile(profile).ok().map(|p| p.scope())
    }

    fn create_conversation<'a>(
        &'a self,
        profile: &'a str,
        conversation: Conversation,
    ) -> HostFuture<'a, String> {
        Box::pin(async move {
            let profile = &self.profile(profile)?;
            let cwd = Self::cwd_of(profile, None);
            std::fs::create_dir_all(&cwd)
                .map_err(|e| Error::io(&pmt_core::error::codes::HOME_IO, &cwd, &e))?;
            let setup = conversation
                .policy
                .as_ref()
                .map(|p| pmt_gateway::messaging::policy::session_setup(p, &conversation.platform))
                .unwrap_or_default();
            let factory = self.factory(profile, cwd.clone(), &setup)?;
            let target = factory.models.primary(&setup::flags(&setup)).await?;
            let (source, kind) = if conversation.background {
                (Source::Background, LineageKind::Background)
            } else {
                (Source::Gateway, LineageKind::Reset)
            };
            let mut new = match &conversation.parent {
                Some(parent) => NewSession::child(source, session_id(parent)?, kind),
                None => NewSession::new(source),
            };
            new.origin = Origin::Gateway {
                platform: conversation.platform,
                chat: conversation.chat_id,
                thread: conversation.thread_id,
                user: Some(conversation.user_id),
            };
            new.provider = Some(target.provider.clone());
            new.model.clone_from(&target.model);
            new.cwd = Some(cwd.to_string_lossy().into_owned());
            let id = profile.store.create_session(&new)?.id;
            if setup != SessionSetup::default() {
                Self::set_up(profile, id, setup).await?;
            }
            let preload = conversation
                .policy
                .as_ref()
                .and_then(|p| p.preload_skills.clone())
                .unwrap_or_default();
            if !preload.is_empty() {
                let home = self.launch.user_home().map(PathBuf::as_path);
                preload_context(&profile.scope(), &profile.store, home, id, &preload)?;
            }
            Ok(id.to_string())
        })
    }

    fn steer<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
        text: &'a str,
    ) -> HostFuture<'a, bool> {
        Box::pin(async move {
            let profile = &self.profile(profile)?;
            let id = self::session_id(session_id)?;
            let loaded = profile.loaded.lock().await.get(&id).cloned();
            match loaded {
                Some(session) if session.busy.load(Ordering::Acquire) => {
                    session.live.agent.steer(text)?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
    }

    fn lookup<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
        line: &'a str,
    ) -> HostFuture<'a, Lookup> {
        Box::pin(async move {
            let profile = &self.profile(profile)?;
            let scope = profile.scope();
            let loaded = match self::session_id(session_id) {
                Ok(id) => profile.loaded.lock().await.get(&id).cloned(),
                Err(_) => None,
            };
            if !line.trim_start().starts_with('/') {
                // An input answering what the session waits for (the `yes`
                // to a `/cron` echo, a `/blueprint` answer) runs as
                // `/cron` (its access rules and busy policy).
                let awaited = loaded
                    .as_ref()
                    .is_some_and(|s| commands::cron::awaits(s.live.id, line));
                if !awaited {
                    return Ok(Lookup::Text);
                }
            }
            let registry = match &loaded {
                Some(session) => commands::registry(&scope, &session.live),
                None => commands::profile_registry(&scope),
            };
            if !line.trim_start().starts_with('/') {
                return Ok(lookup_of(registry.resolve("/cron", Surface::Gateway)));
            }
            Ok(lookup_of(registry.resolve(line, Surface::Gateway)))
        })
    }

    fn run_command<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
        input: &'a str,
        link: Link,
    ) -> HostFuture<'a, Dispatched> {
        let span = session_span(profile, session_id);
        Box::pin(
            async move {
                let profile = &self.profile(profile)?;
                self.gateway_command(profile, session_id, input, link).await
            }
            .instrument(span),
        )
    }

    fn evict<'a>(&'a self, profile: &'a str, session_id: &'a str) -> HostFuture<'a, bool> {
        Box::pin(
            async move {
                let profile = &self.profile(profile)?;
                Ok(Self::unload(profile, self::session_id(session_id)?).await)
            }
            .instrument(session_span(profile, session_id)),
        )
    }

    fn serve_new_profiles(&self) -> Vec<String> {
        self.open_new_profiles()
    }

    fn menu_commands(&self, profile: &str) -> Vec<MenuCommand> {
        let Ok(profile) = self.profile(profile) else {
            return Vec::new();
        };
        menu_commands(
            &profile.scope(),
            &profile.store,
            self.launch.user_home().map(PathBuf::as_path),
        )
    }

    fn append_notice<'a>(
        &'a self,
        profile: &'a str,
        session: &'a str,
        kind: &'a str,
        text: &'a str,
    ) -> HostFuture<'a, ()> {
        Box::pin(async move {
            let profile = &self.profile(profile)?;
            let row = pmt_agent::turn::transcript::notice_row(text, kind);
            profile.store.append_message(session_id(session)?, &row)?;
            Ok(())
        })
    }

    fn admit_background<'a>(
        &'a self,
        profile: &'a str,
        session: &'a str,
    ) -> HostFuture<'a, pmt_gateway::messaging::agents::BackgroundSlot> {
        Box::pin(async move {
            let scope = self.profile(profile)?.scope();
            let delegation = &scope.config().delegation;
            let admission = pmt_delegation::limiter::Limiter::for_profile(
                scope.dir(),
                delegation.max_concurrent_children,
                delegation.max_queued,
            )
            .admit(session, 1)
            .map_err(|refusal| refusal.error())?
            .pop()
            .expect("one admission per task");
            let permit = admission.ready().await;
            Ok(Box::new(permit) as pmt_gateway::messaging::agents::BackgroundSlot)
        })
    }

    fn speech(&self, profile: &str) -> Option<Arc<dyn pmt_gateway::messaging::voice::Speech>> {
        super::speech::of_profile(&self.scope(profile)?)
    }

    fn model_choices(&self, profile: &str) -> Vec<ModelChoice> {
        let Ok(profile) = self.profile(profile) else {
            return Vec::new();
        };
        let scope = profile.scope();
        let home = self.launch.user_home().map(PathBuf::as_path);
        match crate::model_cmd::Models::for_config(scope.clone(), scope.config(), home) {
            Ok(models) => crate::model_cmd::picker::chat_choices(&models),
            Err(err) => {
                tracing::warn!("the model picker has no providers: {err}");
                Vec::new()
            }
        }
    }

    fn update_from_chat<'a>(&'a self, request: &'a UpdateFromChat) -> HostFuture<'a, UpdateRun> {
        Box::pin(async move {
            let home = self
                .profile(&self.launch_profile)?
                .scope()
                .home()
                .to_path_buf();
            let request = request.clone();
            pmt_core::spawn::blocking(move || super::super::update_chat::run(&home, &request))
                .await
                .map_err(|e| Error::new(&pmt_core::error::codes::RPC_INTERNAL, e.to_string()))?
        })
    }

    fn take_automated<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
    ) -> HostFuture<'a, pmt_gateway::messaging::automation::AfterTurn> {
        Box::pin(async move { Ok(self.take_after(profile, session_id).await) })
    }

    fn find_cron_job<'a>(&'a self, profile: &'a str, job: &'a str) -> HostFuture<'a, String> {
        Box::pin(async move { self.cron_job_id(profile, job) })
    }

    fn fire_cron_job<'a>(
        &'a self,
        profile: &'a str,
        job: &'a str,
        context: &'a str,
    ) -> HostFuture<'a, pmt_gateway::messaging::CronFire> {
        Box::pin(async move { self.fire_cron(profile, job, context) })
    }

    fn end_conversation<'a>(&'a self, profile: &'a str, session_id: &'a str) -> HostFuture<'a, ()> {
        Box::pin(
            async move {
                let profile = &self.profile(profile)?;
                let id = self::session_id(session_id)?;
                Self::close_loaded(profile, id, EndReason::NewSession).await;
                // A session that was not loaded (or is held right now) is
                // ended in the store; the first end wins.
                profile.store.end_session(id, EndReason::NewSession)?;
                Ok(())
            }
            .instrument(session_span(profile, session_id)),
        )
    }
}

impl DaemonHost {
    /// Serves the profiles created since start when the launch profile
    /// multiplexes (spec: configuration-and-profiles, Served profile set;
    /// messaging-gateway, Profiles served by one daemon): each opens in
    /// its own scope; `standalone` ones and ones that fail to load (logged)
    /// are left out. Returns the names now served.
    fn open_new_profiles(&self) -> Vec<String> {
        let Ok(launch) = self.profile(&self.launch_profile) else {
            return Vec::new();
        };
        let scope = launch.scope();
        if !scope.config().gateway.multiplex {
            return Vec::new();
        }
        // Profiles live at `<home>/profiles/<name>` (design D13).
        let Some(root) = scope.dir().parent().and_then(std::path::Path::parent) else {
            return Vec::new();
        };
        let (Ok(home), Ok(overrides)) = (Home::open(root), Overrides::parse(&self.set)) else {
            return Vec::new();
        };
        let mut added = Vec::new();
        for name in home.profiles().unwrap_or_default() {
            if self.served().contains_key(name.as_str()) {
                continue;
            }
            let opened = home
                .existing_profile(&name)
                .and_then(|dir| ProfileScope::open(dir, &overrides, Launch::Other));
            match opened.and_then(|scope| {
                if scope.config().gateway.standalone {
                    return Ok(None);
                }
                crate::startup::register_profile(&scope);
                Profile::open(scope).map(Some)
            }) {
                Ok(Some(profile)) => {
                    self.profiles
                        .write()
                        .expect("profiles poisoned")
                        .insert(name.as_str().to_owned(), profile);
                    added.push(name.as_str().to_owned());
                }
                Ok(None) => {}
                Err(err) => tracing::error!(profile = name.as_str(), "profile not served: {err}"),
            }
        }
        added
    }
}
