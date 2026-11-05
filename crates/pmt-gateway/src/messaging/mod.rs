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
//! - `menus`: native command menus, registered again when the registry
//!   changes;
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
mod admit_groups;
pub mod agents;
mod artifacts;
pub mod attachments;
mod attention;
pub mod auth;
mod cache;
mod cards;
pub mod conformance;
mod dedup;
pub mod deliverable;
pub mod descriptor;
mod gating;
pub mod geo;
pub mod handoff;
pub mod health;
pub mod hooks;
mod http;
mod inbound;
pub mod key;
mod lane;
mod limits;
mod locks;
pub mod markup;
pub mod mcp_events;
mod media;
pub mod menu;
mod menus;
pub(crate) mod methods;
pub mod mock_server;
pub mod multipart;
pub mod operate;
mod outbox;
mod peer;
pub mod plugin_ops;
pub mod policy;
mod profile_routes;
pub mod proxy;
pub mod proxy_env;
mod recovery;
mod relay;
mod restart;
mod retry;
mod send;
pub mod shape;
pub mod sign;
pub mod split;
pub mod target;
pub mod tasks;
mod topics;
mod typing;
pub mod voice;
pub mod ws;

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
    AdapterState, AdapterStatus, Addressing, Admission, Attachment, AudioOp, Button, ChatInfo,
    Descriptor, HistoryEntry, HttpRequest, HttpResponse, Inbound, Interactive, LengthUnit, Markup,
    MediaKind, MediaStream, MenuCommand, Outgoing, Private, Reaction, Select, SendError,
    SenderVerdict, Sent, SessionHints, TaskCard, TaskStatus,
};
pub use agents::{Agents, BusyPolicy, Conversation, Lookup, ModelChoice, UpdateFromChat};
pub use hooks::{Hook, HookBridge, Hooks, PreCommand};
pub use http::HttpOptions;
pub use key::{ChatType, Isolation, KeyParts, Routing, SessionKey};
pub use lane::{Background, BackgroundKind};
pub use recovery::{CLEAN_SHUTDOWN, RESTARTED};
pub use relay::{RelayOptions, RelaySource};
pub use send::{KnownTarget, SendRequest, known_targets};
pub use voice::Speech;

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
    /// The shared HTTP listener of webhook-style adapters
    /// (`daemon.toml gateway.http`).
    pub http: HttpOptions,
    /// Where platform credential locks live (`<user runtime dir>/locks`);
    /// `None` turns them off.
    pub locks_dir: Option<PathBuf>,
    /// `daemon run --replace`: take over credential locks live holders
    /// keep.
    pub replace_locks: bool,
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
            http: HttpOptions::default(),
            locks_dir: None,
            replace_locks: false,
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
    dedup: dedup::Dedup,
    http: http::Listener,
    /// Signs approval card callbacks (fresh at every start).
    card_key: cards::CardKey,
    relay: std::sync::OnceLock<Arc<relay::Relay>>,
    last_inbound: send::LastInbound,
    limits: limits::Limits,
    /// Bot message bursts being collected.
    bursts: gating::Bursts,
    /// Role lookups of `allow_roles` grants.
    roles: gating::Roles,
    /// Observed group lines waiting for the next turn in their chat.
    observed: gating::Observed,
    menus: menus::Menus,
    /// `/background` tasks running.
    tasks: tasks::Tasks,
    /// DM topic lobbies and topic names.
    topics: topics::TopicState,
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

    /// The table of `platform` in `profile`'s configuration now.
    pub(crate) fn platform_config(
        &self,
        profile: &str,
        platform: &str,
    ) -> pmt_config::schema::PlatformConfig {
        self.agents
            .scope(profile)
            .map(|s| s.config().gateway.platform(platform))
            .unwrap_or_default()
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
            dedup: dedup::Dedup::default(),
            http: http::Listener::default(),
            card_key: cards::CardKey::random(),
            relay: std::sync::OnceLock::new(),
            last_inbound: send::LastInbound::default(),
            limits: limits::Limits::default(),
            bursts: gating::Bursts::default(),
            roles: gating::Roles::default(),
            observed: gating::Observed::default(),
            menus: menus::Menus::default(),
            tasks: tasks::Tasks::default(),
            topics: topics::TopicState::default(),
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
        profile_routes::lend_configured(&shared);
        restart::announce(&shared);
        cache::spawn(&shared);
        menus::spawn(&shared);
        attention::spawn(&shared);
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
                .emit(
                    profile,
                    "on_gateway_startup",
                    serde_json::json!({"platforms": enabled_platforms(&shared, profile)}),
                )
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
        let shared = &self.shared;
        let mut all = shared.adapters.statuses();
        for status in &mut all {
            status.paused = operate::is_paused(shared, &status.profile, &status.platform);
        }
        all
    }

    /// The address of the shared webhook listener, once it runs.
    #[must_use]
    pub fn http_addr(&self) -> Option<std::net::SocketAddr> {
        self.shared.http.addr()
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
        profile_routes::lend_configured(shared);
        menus::refresh(shared);
    }

    /// The message length limit of `profile`'s running `platform`
    /// adapter.
    #[must_use]
    pub fn max_message_length(&self, profile: &str, platform: &str) -> Option<usize> {
        self.shared
            .adapters
            .descriptor(profile, platform)
            .map(|d| d.max_message_length)
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
        } else if shared.server.restart_requested() {
            restart::mark_interrupted(&shared.options.home);
        }
    }
}

/// The platforms `profile` enables, by name (`on_gateway_startup`).
fn enabled_platforms(shared: &Shared, profile: &str) -> Vec<String> {
    shared.agents.scope(profile).map_or_else(Vec::new, |scope| {
        scope
            .config()
            .gateway
            .platforms
            .iter()
            .filter(|(_, table)| table.enabled)
            .map(|(name, _)| name.clone())
            .collect()
    })
}
