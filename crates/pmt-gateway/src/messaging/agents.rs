//! What the messaging gateway needs from the agent side beyond the
//! [`Host`] every client uses: the profile's store and configuration,
//! conversation sessions with a messaging origin, steering, the shared
//! command registry as the gateway surface sees it, and unloading an idle
//! agent without ending its session.

use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_rpc::contracts::Dispatched;
use pmt_store::Store;

use crate::host::{Host, HostFuture, Link};

/// A command's declared behaviour while a turn runs (spec: cli, Busy
/// policy enforcement; messaging-gateway, Command busy policy declared in
/// the registry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyPolicy {
    /// Refused mid-turn.
    Reject,
    /// Runs after the turn.
    Queue,
    /// Stops the turn, then runs.
    InterruptThenDispatch,
    /// Runs at once, alongside the turn.
    Immediate,
}

/// What a slash line is on the gateway surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// Not a known command: ordinary text.
    Text,
    /// Nothing runs; the reply says why (ambiguous prefix, not on this
    /// surface).
    Reply(String),
    /// A registry command with the gateway surface.
    Command {
        /// Canonical name.
        name: String,
        /// Everything after the command token.
        args: String,
        /// Its declared busy policy.
        busy: BusyPolicy,
    },
}

/// A conversation session to create.
#[derive(Debug, Clone, PartialEq)]
pub struct Conversation {
    /// Platform name.
    pub platform: String,
    /// Chat id.
    pub chat_id: String,
    /// Thread id.
    pub thread_id: Option<String>,
    /// The sender who started it.
    pub user_id: String,
    /// The session it replaces (`/new`, suspension), or for a background
    /// task the chat's session that launched it.
    pub parent: Option<String>,
    /// A `/bg` task's own session (spec: subagent-delegation, User-launched
    /// background sessions): recorded with `source = background`, its
    /// `parent` linked for lineage only.
    pub background: bool,
    /// The chat policy the session is created with (frozen with it).
    pub policy: Option<pmt_config::schema::ChatPolicy>,
}

/// A background task's concurrency slot, held while it runs (dropping it
/// frees the slot).
pub type BackgroundSlot = Box<dyn std::any::Any + Send>;

/// A configured provider and the models a chat's picker offers of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    /// Provider id.
    pub provider: String,
    /// Its models, as `/model <provider>/<model>` names them.
    pub models: Vec<String>,
}

/// `/update [check]` from a chat, once confirmed (spec:
/// distribution-and-update, Update from chat).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateFromChat {
    /// The profile whose chat asked.
    pub profile: String,
    /// The chat's platform.
    pub platform: String,
    /// The chat.
    pub chat_id: String,
    /// Its thread.
    pub thread_id: Option<String>,
    /// Only look for a newer release.
    pub check: bool,
}

/// The agent side of the messaging gateway.
pub trait Agents: Host {
    /// The store of a served profile.
    fn store(&self, profile: &str) -> Option<Arc<Store>>;

    /// The current configuration of a served profile.
    fn scope(&self, profile: &str) -> Option<ProfileScope>;

    /// Creates a messaging session (source `gateway`, its chat as origin,
    /// the replaced session as parent).
    fn create_conversation<'a>(
        &'a self,
        profile: &'a str,
        conversation: Conversation,
    ) -> HostFuture<'a, String>;

    /// Hands `text` to the running turn before its next model call;
    /// `false` when no turn runs.
    fn steer<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
        text: &'a str,
    ) -> HostFuture<'a, bool>;

    /// Resolves a slash line against the shared registry for the gateway
    /// surface (quick commands included).
    fn lookup<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
        line: &'a str,
    ) -> HostFuture<'a, Lookup>;

    /// Runs a registry command on the gateway surface; its notices reach
    /// the session through `link`.
    fn run_command<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
        line: &'a str,
        link: Link,
    ) -> HostFuture<'a, Dispatched>;

    /// Unloads the session's agent without ending the session (it is
    /// rebuilt from the store on its next message); `false` when it runs a
    /// turn or holds queued input.
    fn evict<'a>(&'a self, profile: &'a str, session_id: &'a str) -> HostFuture<'a, bool>;

    /// Starts serving the profiles of the home created since the daemon
    /// started (with a multiplexing launch profile; `standalone` ones are
    /// left out) and returns their names (`gateway reload`).
    fn serve_new_profiles(&self) -> Vec<String>;

    /// Ends a session the gateway replaced (`/new`, `/reset`, stuck-loop
    /// suspension).
    fn end_conversation<'a>(&'a self, profile: &'a str, session_id: &'a str) -> HostFuture<'a, ()>;

    /// The mounts of `session`'s container, when its terminal runs in
    /// one (its reply's container paths are then translated to host
    /// paths).
    fn container_mounts(
        &self,
        _profile: &str,
        _session: &str,
    ) -> Option<crate::messaging::attachments::ContainerMounts> {
        None
    }
    /// The gateway-surface commands and user-invocable skills of
    /// `profile`, for native command menus.
    fn menu_commands(&self, _profile: &str) -> Vec<crate::messaging::adapter::MenuCommand> {
        Vec::new()
    }
    /// Appends a notice row `text` of `kind` to `session`'s transcript
    /// (the model reads it as context; the user did not type it).
    fn append_notice<'a>(
        &'a self,
        profile: &'a str,
        session: &'a str,
        kind: &'a str,
        text: &'a str,
    ) -> HostFuture<'a, ()> {
        Box::pin(async move {
            let store = self.store(profile).ok_or_else(|| {
                pmt_core::error::Error::new(
                    &pmt_core::error::codes::PROFILE_NOT_FOUND,
                    format!("profile '{profile}' is not served"),
                )
            })?;
            let mut row = pmt_store::NewMessage::new(pmt_store::Role::User, text);
            row.metadata = Some(serde_json::json!({ "kind": kind }));
            store.append_message(session.parse()?, &row)?;
            Ok(())
        })
    }
    /// Waits for a slot of the profile's child concurrency limit for the
    /// background task of `session` (spec: subagent-delegation,
    /// User-launched background sessions count against
    /// `delegation.max_concurrent_children`). The default admits at once.
    fn admit_background<'a>(
        &'a self,
        _profile: &'a str,
        _session: &'a str,
    ) -> HostFuture<'a, BackgroundSlot> {
        Box::pin(async { Ok(Box::new(()) as BackgroundSlot) })
    }
    /// The providers configured for `profile` and their models, for the
    /// chat model picker (`/model` without arguments).
    fn model_choices(&self, _profile: &str) -> Vec<ModelChoice> {
        Vec::new()
    }
    /// The profile's speech providers, when it has any (voice messages
    /// are then transcribed and replies may be spoken).
    fn speech(&self, _profile: &str) -> Option<Arc<dyn crate::messaging::voice::Speech>> {
        None
    }
    /// Runs the updater for a confirmed `/update` (or `/update check`):
    /// the text answers the chat.
    fn update_from_chat<'a>(&'a self, _request: &'a UpdateFromChat) -> HostFuture<'a, String> {
        Box::pin(async {
            Err(pmt_core::error::Error::new(
                &pmt_core::error::codes::GATEWAY_DAEMON_REQUIRED,
                "updates from chat need the daemon's updater",
            ))
        })
    }
}
