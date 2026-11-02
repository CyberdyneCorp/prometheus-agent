//! The messaging gateway (spec: messaging-gateway): every served profile's
//! messaging adapters, the conversations they carry and the durable
//! delivery of every reply.
//!
//! - [`key`]: routing identity and deterministic conversation keys;
//! - [`adapter`]: what an adapter declares, submits and is asked to send;
//! - [`agents`]: what the gateway needs from the agent side;
//! - [`auth`]: the authorization order, command tiers and bot-loop guard;
//! - [`hooks`]: typed gateway hooks;
//! - `admit`: an inbound event's way to its conversation lane;
//! - `lane`: one conversation (session routing, busy input, commands,
//!   turns, approvals, streaming);
//! - `outbox`: the delivery worker;
//! - `adapters`: adapter connection, reconnection and state;
//! - `recovery`: the clean-shutdown marker and crash recovery;
//! - `cache`: live agents kept in memory.
//!
//! The gateway is one internal client of the daemon's hub: it attaches to
//! the sessions it routes as a driver that answers server requests, so a
//! turn's events and approval prompts reach the chat and every attached
//! client alike, and the first valid answer from any of them wins.

pub mod adapter;
mod adapters;
mod admit;
pub mod agents;
pub mod auth;
mod cache;
pub mod health;
pub mod hooks;
pub mod key;
mod lane;
mod limits;
pub(crate) mod methods;
mod outbox;
mod peer;
pub mod policy;
mod recovery;
mod relay;
mod send;
pub mod target;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_config::daemon::AllAdaptersDown;
use pmt_config::schema::{AgentCacheConfig, OutboxConfig};
use pmt_core::error::Error;
use pmt_store::Store;
use pmt_store::gateway::outbox::NewOutbound;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

pub use adapter::{
    AccessPolicy, Adapter, AdapterError, AdapterFactory, AdapterFuture, AdapterLink, AdapterSetup,
    AdapterState, AdapterStatus, Admission, Attachment, Button, Descriptor, Inbound, Interactive,
    MediaKind, Outgoing, SendError, Sent,
};
pub use agents::{Agents, BusyPolicy, Conversation, Lookup};
pub use hooks::{Hook, Hooks, PreCommand};
pub use key::{ChatType, Isolation, KeyParts, Routing, SessionKey};
pub use lane::{Background, BackgroundKind};
pub use recovery::{CLEAN_SHUTDOWN, RESTARTED};
pub use relay::{RelayOptions, RelaySource};
pub use send::{KnownTarget, SendRequest, known_targets};

use crate::hub::{ConnId, Peer};
use crate::server::Server;

/// How the gateway runs (daemon-wide settings).
#[derive(Clone)]
pub struct Options {
    /// The home (`run/clean_shutdown` lives under it).
    pub home: PathBuf,
    /// Adapter factories by platform name.
    pub factories: BTreeMap<String, AdapterFactory>,
    /// `daemon.toml gateway.on_all_adapters_down`.
    pub on_all_adapters_down: AllAdaptersDown,
    /// `gateway.shutdown.drain_timeout_secs` of the launch profile.
    pub drain_timeout: Duration,
    /// `gateway.agent_cache` of the launch profile.
    pub agent_cache: AgentCacheConfig,
    /// Typed hooks by profile.
    pub hooks: Arc<Hooks>,
    /// The relay connector (`daemon.toml gateway.relay`), when set.
    pub relay: Option<RelayOptions>,
    /// Reads the relay settings again (`gateway reload` connects a relay
    /// enrolled since the daemon started).
    pub relay_source: Option<RelaySource>,
    /// The health export (`observability.gateway_health` of the launch
    /// profile), when enabled.
    pub health: Option<health::HealthOptions>,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("home", &self.home)
            .field("platforms", &self.factories.keys().collect::<Vec<_>>())
            .field("on_all_adapters_down", &self.on_all_adapters_down)
            .field("drain_timeout", &self.drain_timeout)
            .finish_non_exhaustive()
    }
}

impl Options {
    /// Defaults for `home`: no adapters.
    #[must_use]
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            factories: BTreeMap::new(),
            on_all_adapters_down: AllAdaptersDown::StayAlive,
            drain_timeout: Duration::from_secs(30),
            agent_cache: AgentCacheConfig::default(),
            hooks: Arc::new(Hooks::default()),
            relay: None,
            relay_source: None,
            health: None,
        }
    }
}

/// State every part of the gateway shares.
pub(crate) struct Shared {
    server: Arc<Server>,
    agents: Arc<dyn Agents>,
    options: Options,
    /// The gateway's own hub connection and how the hub reaches it.
    conn: ConnId,
    peer: Peer,
    lanes: Mutex<HashMap<String, lane::Handle>>,
    /// Session id to conversation key.
    sessions: Mutex<HashMap<String, String>>,
    adapters: adapters::Adapters,
    guard: auth::BotLoopGuard,
    relay: std::sync::OnceLock<Arc<relay::Relay>>,
    last_inbound: send::LastInbound,
    limits: limits::Limits,
    health: health::Counters,
    cache: cache::AgentCache,
    /// Unserved-profile warnings already logged, by adapter, with when.
    unserved: Mutex<HashMap<String, tokio::time::Instant>>,
    outbox_wake: Notify,
    stop: CancellationToken,
}

impl Shared {
    /// The lane of the conversation `session` belongs to, when it has one.
    pub(crate) fn lane_of_session(&self, session: &str) -> Option<lane::Handle> {
        let key = self
            .sessions
            .lock()
            .expect("sessions poisoned")
            .get(session)
            .cloned()?;
        self.lanes
            .lock()
            .expect("lanes poisoned")
            .get(&key)
            .cloned()
    }

    /// Conversations with a lane, and how many run a turn.
    fn lane_counts(&self) -> (u64, u64) {
        let lanes = self.lanes.lock().expect("lanes poisoned");
        let running = lanes.values().filter(|h| h.running()).count();
        (lanes.len() as u64, running as u64)
    }

    fn store(&self, profile: &str) -> Option<Arc<Store>> {
        self.agents.store(profile)
    }

    /// Stores an outbound message and wakes the delivery worker.
    pub(crate) fn deliver(&self, profile: &str, new: &NewOutbound) {
        let Some(store) = self.store(profile) else {
            return;
        };
        match store.enqueue_outbound(new) {
            Ok(_) => self.outbox_wake.notify_waiters(),
            Err(err) => tracing::error!(profile, "cannot store an outbound message: {err}"),
        }
    }

    pub(crate) fn outbox_config(&self, profile: &str) -> OutboxConfig {
        self.agents
            .scope(profile)
            .map(|s| s.config().gateway.outbox.clone())
            .unwrap_or_default()
    }
}

/// The message that tells a newly paired sender they are in (queued in
/// the profile's outbox by every approval path: the CLI, the RPC and
/// `/pair approve`).
#[must_use]
pub fn paired_notice(pairing: &pmt_store::gateway::pairing::Pairing) -> NewOutbound {
    NewOutbound {
        idempotency_key: format!(
            "paired:{}:{}:{}",
            pairing.platform,
            pairing.sender_id,
            pairing
                .approved_at
                .map_or(0, jiff::Timestamp::as_millisecond)
        ),
        session_id: None,
        platform: pairing.platform.clone(),
        chat_id: pairing.chat_id.clone(),
        thread_id: None,
        reply_to: None,
        edit_of: None,
        kind: pmt_store::gateway::outbox::OutboundKind::System,
        body: "You are paired with this assistant now; send a message to start.".to_owned(),
        attachments: Vec::new(),
    }
}

/// The running messaging gateway.
#[derive(Clone)]
pub struct Gateway {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway").finish_non_exhaustive()
    }
}

impl Gateway {
    /// Starts the gateway for every profile `server` serves: crash
    /// recovery (unless the last stop was clean), the delivery workers,
    /// then each profile's enabled adapters.
    ///
    /// # Errors
    ///
    /// `gateway.recovery` when the clean-shutdown marker cannot be
    /// removed (the daemon exits 1), store errors.
    pub async fn start(
        server: Arc<Server>,
        agents: Arc<dyn Agents>,
        options: Options,
    ) -> Result<Self, Error> {
        let clean = recovery::take_marker(&options.home)?;
        let conn = server.next_conn_id();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let peer = Peer {
            tx,
            minor: server.protocol().minor,
            advertised: Arc::default(),
            server_requests: true,
            client_id: format!("gateway-{conn}"),
            role: pmt_rpc::contracts::AttachRole::Driver,
        };
        let shared = Arc::new(Shared {
            peer,
            adapters: adapters::Adapters::default(),
            guard: auth::BotLoopGuard::default(),
            relay: std::sync::OnceLock::new(),
            last_inbound: send::LastInbound::default(),
            limits: limits::Limits::default(),
            health: health::Counters::default(),
            cache: cache::AgentCache::new(options.agent_cache.clone()),
            lanes: Mutex::default(),
            sessions: Mutex::default(),
            unserved: Mutex::default(),
            outbox_wake: Notify::new(),
            stop: CancellationToken::new(),
            server: Arc::clone(&server),
            agents,
            options,
            conn,
        });
        tokio::spawn(peer::read(Arc::clone(&shared), rx).in_current_span());
        let profiles = server.profiles();
        if !clean {
            for profile in &profiles {
                recovery::recover(&shared, profile);
            }
        }
        for profile in &profiles {
            outbox::spawn(&shared, profile);
        }
        cache::spawn(&shared);
        if let Some(health) = shared.options.health.clone() {
            health::spawn(&shared, health);
        }
        shared
            .health
            .add("prometheus.gateway.lifecycle", &[("event", "startup")]);
        if let Some(options) = shared.options.relay.clone() {
            relay::start(&shared, options).await;
        }
        for profile in &profiles {
            shared
                .options
                .hooks
                .emit(profile, "on_gateway_startup", serde_json::json!({}))
                .await;
            adapters::start_profile(&shared, profile);
        }
        server.set_messaging(Self {
            shared: Arc::clone(&shared),
        });
        Ok(Self { shared })
    }

    /// Submits an inbound event from `platform` of `profile` (what an
    /// adapter's link does).
    pub async fn submit(&self, profile: &str, platform: &str, inbound: Inbound) -> Admission {
        admit::admit(&self.shared, profile, platform, inbound).await
    }

    /// Every adapter's state.
    #[must_use]
    pub fn statuses(&self) -> Vec<AdapterStatus> {
        self.shared.adapters.statuses()
    }

    /// Approves a pending pairing code of `profile` and queues the
    /// sender's welcome (`pairing.approve`).
    ///
    /// # Errors
    ///
    /// `gateway.pairing_locked`, `gateway.pairing_unknown`, store errors.
    pub fn approve_pairing(
        &self,
        profile: &str,
        platform: &str,
        code: &str,
    ) -> Result<pmt_store::gateway::pairing::Pairing, Error> {
        let store = self.shared.store(profile).ok_or_else(|| {
            Error::new(
                &pmt_core::error::codes::PROFILE_NOT_FOUND,
                format!("profile '{profile}' is not served"),
            )
        })?;
        let pairing = store.approve_pairing(platform, code, jiff::Timestamp::now())?;
        self.shared.deliver(profile, &paired_notice(&pairing));
        Ok(pairing)
    }

    /// `gateway reload`: serves the profiles created since start, then
    /// starts the adapters served profiles enable that are not running;
    /// running adapters and sessions are untouched.
    pub fn reload(&self) {
        let shared = &self.shared;
        relay::reload(shared);
        for profile in shared.agents.serve_new_profiles() {
            if shared.server.add_profile(&profile) {
                tracing::info!(profile, "now serving a profile created since start");
                outbox::spawn(shared, &profile);
            }
        }
        for profile in &shared.server.profiles() {
            adapters::start_profile(shared, profile);
        }
    }

    /// Injects a background notification into a conversation (spec:
    /// messaging-gateway, Background and internal notifications); the
    /// admission says whether it was taken (`dropped`: retry later).
    #[must_use]
    pub fn notify(&self, profile: &str, session_id: &str, event: lane::Background) -> Admission {
        lane::notify(&self.shared, profile, session_id, event)
    }

    /// Stops admitting messages, gives running conversations
    /// `gateway.shutdown.drain_timeout_secs` to finish, writes the
    /// clean-shutdown marker when they did, and disconnects the adapters.
    pub async fn shutdown(&self) {
        let shared = &self.shared;
        shared
            .health
            .add("prometheus.gateway.lifecycle", &[("event", "shutdown")]);
        shared.stop.cancel();
        let drained = lane::drain(shared, shared.options.drain_timeout).await;
        for profile in &shared.server.profiles() {
            shared
                .options
                .hooks
                .emit(
                    profile,
                    "on_gateway_shutdown",
                    serde_json::json!({"clean": drained}),
                )
                .await;
        }
        adapters::disconnect_all(shared).await;
        if drained {
            recovery::write_marker(&shared.options.home);
        }
    }
}
