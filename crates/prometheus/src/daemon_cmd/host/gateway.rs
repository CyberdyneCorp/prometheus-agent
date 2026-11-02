//! What the messaging gateway needs from the daemon host (spec:
//! messaging-gateway, Profiles served by one daemon; Agent cache and
//! eviction; Gateway slash commands, quick commands and hooks): each
//! served profile's store and configuration, conversation sessions with a
//! messaging origin, steering, the shared command registry seen from the
//! gateway surface, and unloading an idle agent without ending its
//! session.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use pmt_config::{Home, Launch, Overrides, ProfileScope};
use pmt_core::error::Error;
use pmt_gateway::messaging::{Agents, BusyPolicy, Conversation, Lookup};
use pmt_gateway::{HostFuture, Link};
use pmt_rpc::contracts::{Dispatched, SessionSetup};
use pmt_store::{EndReason, LineageKind, NewSession, Origin, Source, Store};
use tracing::Instrument as _;

use super::{DaemonHost, Profile, session_id, session_span, setup};
use crate::chat::commands;
use crate::slash::{self, Resolution, Surface};

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
            let mut new = match &conversation.parent {
                Some(parent) => {
                    NewSession::child(Source::Gateway, session_id(parent)?, LineageKind::Reset)
                }
                None => NewSession::new(Source::Gateway),
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
            let registry = match &loaded {
                Some(session) => commands::registry(&scope, &session.live),
                None => commands::profile_registry(&scope),
            };
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
