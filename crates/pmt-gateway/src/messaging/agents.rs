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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    /// Platform name.
    pub platform: String,
    /// Chat id.
    pub chat_id: String,
    /// Thread id.
    pub thread_id: Option<String>,
    /// The sender who started it.
    pub user_id: String,
    /// The session it replaces (`/new`, suspension).
    pub parent: Option<String>,
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
}
