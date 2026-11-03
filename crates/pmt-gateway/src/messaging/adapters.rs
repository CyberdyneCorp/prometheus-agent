//! Adapter connections (spec: messaging-gateway, Profiles served by one
//! daemon; All adapters down policy; platform-adapters, Adapter state
//! reporting and reconnect): each served profile starts exactly the
//! adapters its own `config.toml` enables; retryable failures reconnect
//! with backoff from 1 second up to 300 seconds; once every adapter has
//! failed for good, `on_all_adapters_down = "exit"` stops the daemon
//! (exit 78) while `stay_alive` keeps serving local clients.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use pmt_config::daemon::AllAdaptersDown;
use serde_json::json;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use super::Shared;
use super::adapter::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterSetup, AdapterState, AdapterStatus,
    Admission, Descriptor, Inbound, Outgoing, SendError, Sent,
};
use super::locks::{self, CredentialLock, Holder};
use super::relay::{Relay, RelayDescriptor, RelayView};

/// First reconnect delay.
const RECONNECT_START: Duration = Duration::from_secs(1);
/// Longest reconnect delay.
const RECONNECT_MAX: Duration = Duration::from_mins(5);

/// What `gateway status` says about a connected adapter whose send path
/// is not yet confirmed.
pub(crate) const UNCONFIRMED: &str =
    "connected but not yet confirmed active; recovering in background";

/// An optional operation a descriptor declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Op {
    Edit,
    Typing,
    Reactions,
    Audio,
}

impl Op {
    fn name(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Typing => "send_typing",
            Self::Reactions => "react",
            Self::Audio => "streaming audio",
        }
    }
}

/// One running adapter.
pub(crate) struct Slot {
    pub(crate) adapter: Arc<dyn Adapter>,
    /// The adapter's descriptor, its limit lowered by the platform table.
    pub(crate) descriptor: Descriptor,
    /// The relay carries this platform.
    pub(crate) relayed: bool,
    status: Mutex<AdapterStatus>,
    /// Declared operations that answered `unsupported` on this
    /// connection: treated as undeclared until the next connect.
    unsupported: Mutex<BTreeSet<Op>>,
    /// The platform credential lock while connected.
    lock: Mutex<Option<CredentialLock>>,
    /// The adapter reported a lost connection.
    lost: Notify,
    stop: CancellationToken,
    /// Where state changes are announced (`adapter.state`).
    shared: Weak<Shared>,
}

impl Slot {
    /// Whether the descriptor declares `op` and it has not answered
    /// `unsupported` on this connection.
    pub(crate) fn can(&self, op: Op) -> bool {
        let declared = match op {
            Op::Edit => self.descriptor.supports_edit,
            Op::Typing => self.descriptor.supports_typing,
            Op::Reactions => self.descriptor.supports_reactions,
            Op::Audio => self.descriptor.streaming_tts,
        };
        declared
            && !self
                .unsupported
                .lock()
                .expect("slot poisoned")
                .contains(&op)
    }

    /// `op` answered `unsupported`: undeclared for the rest of the
    /// connection, logged once.
    pub(crate) fn refused(&self, op: Op) {
        if self.unsupported.lock().expect("slot poisoned").insert(op) {
            let status = self.status.lock().expect("status poisoned");
            tracing::warn!(
                profile = status.profile,
                platform = status.platform,
                "the adapter declared {} but does not support it; not used until it reconnects",
                op.name()
            );
        }
    }

    fn state(&self) -> AdapterState {
        self.status.lock().expect("status poisoned").state
    }

    fn set(&self, state: AdapterState, error: Option<&AdapterError>) {
        let announced = {
            let mut status = self.status.lock().expect("status poisoned");
            let changed = status.state != state || status.code.as_ref() != error.map(|e| &e.code);
            status.state = state;
            status.code = error.map(|e| e.code.clone());
            status.message = error.map(|e| e.message.clone());
            changed.then(|| status.clone())
        };
        if let (Some(status), Some(shared)) = (announced, self.shared.upgrade()) {
            announce(&shared, &status);
        }
    }
}

/// Every running adapter, by profile and platform; and the state of each
/// enabled one that could not be built.
#[derive(Default)]
pub(crate) struct Adapters {
    slots: RwLock<BTreeMap<(String, String), Arc<Slot>>>,
    failed: RwLock<BTreeMap<(String, String), AdapterStatus>>,
}

impl Adapters {
    fn slot(&self, profile: &str, platform: &str) -> Option<Arc<Slot>> {
        self.slots
            .read()
            .expect("adapters poisoned")
            .get(&(profile.to_owned(), platform.to_owned()))
            .cloned()
    }

    /// Whether the relay carries `platform` for `profile` (`None`: no
    /// adapter runs for it).
    pub(crate) fn slot_relayed(&self, profile: &str, platform: &str) -> Option<bool> {
        self.slot(profile, platform).map(|s| s.relayed)
    }

    /// The descriptor of a running adapter.
    pub(crate) fn descriptor(&self, profile: &str, platform: &str) -> Option<Descriptor> {
        self.slot(profile, platform).map(|s| s.descriptor.clone())
    }

    /// The running adapter itself.
    pub(crate) fn adapter(&self, profile: &str, platform: &str) -> Option<Arc<dyn Adapter>> {
        self.slot(profile, platform).map(|s| Arc::clone(&s.adapter))
    }

    /// The adapter when it is connected (`connected` or `degraded`).
    pub(crate) fn ready(&self, profile: &str, platform: &str) -> Option<Arc<Slot>> {
        self.slot(profile, platform)
            .filter(|s| matches!(s.state(), AdapterState::Connected | AdapterState::Degraded))
    }

    /// A send succeeded: a degraded adapter is confirmed active.
    pub(crate) fn confirm(&self, profile: &str, platform: &str) {
        if let Some(slot) = self.slot(profile, platform)
            && slot.state() == AdapterState::Degraded
        {
            slot.set(AdapterState::Connected, None);
        }
    }

    /// Sends outside the outbox (pairing codes, which are stored nowhere,
    /// and streamed drafts).
    pub(crate) async fn send_direct(
        &self,
        profile: &str,
        platform: &str,
        message: &Outgoing,
    ) -> Result<Sent, SendError> {
        let slot = self
            .ready(profile, platform)
            .ok_or_else(|| SendError::Network("the adapter is not connected".to_owned()))?;
        let message = super::markup::rendered(slot.adapter.as_ref(), message);
        let sent = slot.adapter.send(&message).await?;
        self.confirm(profile, platform);
        Ok(sent)
    }

    /// Edits a message outside the outbox (streamed drafts).
    pub(crate) async fn edit_direct(
        &self,
        profile: &str,
        platform: &str,
        message_id: &str,
        message: &Outgoing,
    ) -> Result<(), SendError> {
        let slot = self
            .ready(profile, platform)
            .ok_or_else(|| SendError::Network("the adapter is not connected".to_owned()))?;
        let message = super::markup::rendered(slot.adapter.as_ref(), message);
        slot.adapter.edit(message_id, &message).await
    }

    /// Every adapter's state.
    pub(crate) fn statuses(&self) -> Vec<AdapterStatus> {
        let mut all: Vec<AdapterStatus> = self
            .slots
            .read()
            .expect("adapters poisoned")
            .values()
            .map(|s| s.status.lock().expect("status poisoned").clone())
            .collect();
        all.extend(
            self.failed
                .read()
                .expect("adapters poisoned")
                .values()
                .cloned(),
        );
        all.sort_by(|a, b| (&a.profile, &a.platform).cmp(&(&b.profile, &b.platform)));
        all
    }

    fn all_fatal(&self) -> bool {
        let statuses = self.statuses();
        !statuses.is_empty() && statuses.iter().all(|s| s.state == AdapterState::Fatal)
    }
}

/// Starts the adapters `profile` enables that are not running yet, and
/// the relay's platforms for it.
pub(crate) fn start_profile(shared: &Arc<Shared>, profile: &str) {
    let Some(scope) = shared.agents.scope(profile) else {
        return;
    };
    if let Some(relay) = shared.relay.get()
        && let Some(descriptor) = relay.descriptor()
    {
        front_profile(shared, relay, &descriptor, profile);
    }
    for (platform, table) in &scope.config().gateway.platforms {
        if !table.enabled || shared.adapters.slot(profile, platform).is_some() {
            continue;
        }
        if shared
            .relay
            .get()
            .is_some_and(|r| r.replaces_direct(platform))
        {
            tracing::info!(
                profile,
                platform,
                "the relay fronts {platform}; the direct adapter is not connected \
                 (list it in gateway.relay.allow_direct_platforms to keep it)"
            );
            continue;
        }
        if table.allow_all {
            tracing::warn!(
                profile,
                platform,
                "gateway.platforms.{platform}.allow_all admits every sender"
            );
        }
        let key = (profile.to_owned(), platform.clone());
        let Some(factory) = shared.options.factories.get(platform) else {
            let error = AdapterError {
                retryable: false,
                code: "unknown_platform".to_owned(),
                message: format!("this build has no adapter for platform '{platform}'"),
            };
            record_failure(shared, key, &error);
            continue;
        };
        let setup = AdapterSetup {
            scope: scope.clone(),
            platform: platform.clone(),
            config: table.clone(),
        };
        match factory(&setup) {
            Ok(adapter) => launch(shared, key, adapter),
            Err(err) => {
                let error = AdapterError {
                    retryable: false,
                    code: err.code().code.to_owned(),
                    message: err.message().to_owned(),
                };
                record_failure(shared, key, &error);
            }
        }
    }
}

fn record_failure(shared: &Shared, key: (String, String), error: &AdapterError) {
    tracing::error!(
        profile = key.0,
        platform = key.1,
        "adapter not started: {}",
        error.message
    );
    let status = AdapterStatus {
        profile: key.0.clone(),
        platform: key.1.clone(),
        state: AdapterState::Fatal,
        code: Some(error.code.clone()),
        message: Some(error.message.clone()),
        descriptor: None,
    };
    shared
        .adapters
        .failed
        .write()
        .expect("adapters poisoned")
        .insert(key, status);
    check_all_down(shared);
}

fn launch(shared: &Arc<Shared>, key: (String, String), adapter: Arc<dyn Adapter>) {
    launch_slot(shared, key, adapter, false);
}

fn launch_slot(
    shared: &Arc<Shared>,
    key: (String, String),
    adapter: Arc<dyn Adapter>,
    relayed: bool,
) {
    let descriptor = match loaded_descriptor(shared, &key, adapter.as_ref(), relayed) {
        Ok(descriptor) => descriptor,
        Err(error) => return record_failure(shared, key, &error),
    };
    if descriptor.binds_http
        && let Err(message) = super::http::ensure(shared)
    {
        let error = AdapterError {
            retryable: false,
            code: "http_listener".to_owned(),
            message,
        };
        return record_failure(shared, key, &error);
    }
    let slot = Arc::new(Slot {
        adapter,
        status: Mutex::new(AdapterStatus {
            profile: key.0.clone(),
            platform: key.1.clone(),
            state: AdapterState::Connecting,
            code: None,
            message: None,
            descriptor: Some(descriptor.clone()),
        }),
        descriptor,
        relayed,
        unsupported: Mutex::default(),
        lock: Mutex::default(),
        lost: Notify::new(),
        stop: shared.stop.child_token(),
        shared: Arc::downgrade(shared),
    });
    shared
        .adapters
        .slots
        .write()
        .expect("adapters poisoned")
        .insert(key.clone(), Arc::clone(&slot));
    let span = tracing::info_span!("adapter", profile = key.0, platform = key.1);
    tokio::spawn(supervise(Arc::downgrade(shared), key, slot).instrument(span));
}

/// Connects the adapter and reconnects it after a retryable loss.
async fn supervise(shared: Weak<Shared>, key: (String, String), slot: Arc<Slot>) {
    let mut wait = RECONNECT_START;
    loop {
        let Some(outcome) = connect_once(&shared, &key, &slot).await else {
            return;
        };
        match outcome {
            Ok(()) => {
                wait = RECONNECT_START;
                tokio::select! {
                    () = slot.stop.cancelled() => return,
                    () = slot.lost.notified() => {}
                }
            }
            Err(error) if !failed(&shared, &key, &slot, &error).await => return,
            Err(_) => {}
        }
        if slot.state() == AdapterState::Fatal && !slot.lock_wait() {
            release(&slot);
            if let Some(shared) = shared.upgrade() {
                check_all_down(&shared);
            }
            return;
        }
        tokio::select! {
            () = slot.stop.cancelled() => return,
            () = tokio::time::sleep(wait) => {}
        }
        wait = (wait * 2).min(RECONNECT_MAX);
    }
}

/// One connect attempt (after the credential lock); `None` when the
/// adapter was stopped meanwhile. A connected adapter is `degraded` until
/// a send succeeds.
async fn connect_once(
    shared: &Weak<Shared>,
    key: &(String, String),
    slot: &Arc<Slot>,
) -> Option<Result<(), AdapterError>> {
    let link: Arc<dyn AdapterLink> = Arc::new(Link {
        shared: Weak::clone(shared),
        profile: key.0.clone(),
        platform: key.1.clone(),
        slot: Arc::downgrade(slot),
    });
    slot.set(AdapterState::Connecting, None);
    slot.unsupported.lock().expect("slot poisoned").clear();
    // A credential another holder keeps may free up: retried, visibly
    // fatal meanwhile.
    if let Err(error) = take_lock(shared, key, slot) {
        return Some(Err(error));
    }
    let outcome = tokio::select! {
        () = slot.stop.cancelled() => return None,
        outcome = slot.adapter.connect(link) => outcome,
    };
    if outcome.is_ok() && slot.state() == AdapterState::Connecting {
        let unconfirmed = AdapterError {
            retryable: true,
            code: "unconfirmed".to_owned(),
            message: UNCONFIRMED.to_owned(),
        };
        slot.set(AdapterState::Degraded, Some(&unconfirmed));
    }
    if outcome.is_ok() {
        super::menus::register(shared, key, slot);
    }
    Some(outcome)
}

/// Records a failed connect; `false` when the adapter stops for good.
async fn failed(
    shared: &Weak<Shared>,
    key: &(String, String),
    slot: &Slot,
    error: &AdapterError,
) -> bool {
    if error.code == format!("{}_lock", key.1) {
        slot.set(AdapterState::Fatal, Some(error));
        return true;
    }
    if error.retryable {
        slot.set(AdapterState::Retrying, Some(error));
        return true;
    }
    slot.set(AdapterState::Fatal, Some(error));
    release(slot);
    if let Some(shared) = shared.upgrade() {
        platform_event(&shared, key, error).await;
        check_all_down(&shared);
    }
    false
}

impl Slot {
    /// Fatal only because another process holds the credential: retried.
    fn lock_wait(&self) -> bool {
        let status = self.status.lock().expect("status poisoned");
        status
            .code
            .as_deref()
            .is_some_and(|code| code == format!("{}_lock", status.platform))
    }
}

/// Takes the platform credential lock for a connect (no-op when held, or
/// when the adapter's credential is not unique or locks are off).
fn take_lock(
    shared: &Weak<Shared>,
    key: &(String, String),
    slot: &Slot,
) -> Result<(), AdapterError> {
    let Some(shared) = shared.upgrade() else {
        return Ok(());
    };
    let (Some(dir), Some(identity)) = (
        shared.options.locks_dir.as_deref(),
        slot.adapter.credential_identity(),
    ) else {
        return Ok(());
    };
    let mut held = slot.lock.lock().expect("slot poisoned");
    if held.is_none() {
        let holder = Holder::current(&shared.options.home, &key.0);
        let platform = &key.1;
        *held = Some(locks::acquire(
            dir,
            platform,
            &identity,
            holder,
            shared.options.replace_locks,
        )?);
    }
    Ok(())
}

/// Releases the credential lock of a stopped adapter.
fn release(slot: &Slot) {
    slot.lock.lock().expect("slot poisoned").take();
}

/// The adapter's descriptor once checked: a malformed one, upstream
/// authorization outside the relay, or a table raising the limit fails
/// the load; a lower `max_message_length` applies.
fn loaded_descriptor(
    shared: &Shared,
    key: &(String, String),
    adapter: &dyn Adapter,
    relayed: bool,
) -> Result<Descriptor, AdapterError> {
    let refuse = |code: &str, message: String| AdapterError {
        retryable: false,
        code: code.to_owned(),
        message,
    };
    let mut descriptor = adapter.describe();
    descriptor
        .check()
        .map_err(|m| refuse("invalid_descriptor", m))?;
    if descriptor.authorization_is_upstream && !relayed {
        return Err(refuse(
            "invalid_descriptor",
            "descriptor field authorization_is_upstream is reserved to the relay adapter".into(),
        ));
    }
    let lowered = shared
        .agents
        .scope(&key.0)
        .and_then(|scope| scope.config().gateway.platform(&key.1).max_message_length);
    if let Some(limit) = lowered.map(|l| l as usize) {
        if limit > descriptor.max_message_length {
            return Err(refuse(
                "config_invalid",
                format!(
                    "gateway.platforms.{}.max_message_length: the adapter's maximum is {}; a limit can only be lowered",
                    key.1, descriptor.max_message_length
                ),
            ));
        }
        descriptor.max_message_length = limit;
    }
    Ok(descriptor)
}

/// Tells the profile's attached clients an adapter's new state.
fn announce(shared: &Shared, status: &AdapterStatus) {
    let payload = json!({
        "profile": status.profile,
        "platform": status.platform,
        "state": status.state,
        "code": status.code,
        "message": status.message,
    });
    shared
        .server
        .hub()
        .broadcast(&status.profile, "adapter.state", payload, shared.conn);
}

async fn platform_event(shared: &Shared, key: &(String, String), error: &AdapterError) {
    shared.health.add(
        "prometheus.adapter.fatal",
        &[
            ("profile", &key.0),
            ("platform", &key.1),
            ("code", &error.code),
        ],
    );
    shared
        .options
        .hooks
        .emit(
            &key.0,
            "gateway_platform_event",
            json!({"platform": key.1, "kind": "fatal", "code": error.code}),
        )
        .await;
}

/// Stops the daemon when every adapter failed for good and the policy is
/// `exit`; with `stay_alive` the daemon keeps serving local clients.
fn check_all_down(shared: &Shared) {
    if shared.options.on_all_adapters_down == AllAdaptersDown::Exit
        && shared.adapters.all_fatal()
        && !shared.stop.is_cancelled()
    {
        tracing::error!(
            "every messaging adapter failed for good and gateway.on_all_adapters_down = \"exit\""
        );
        shared.server.request_fatal_stop();
    }
}

/// The relay's descriptor arrived: every served profile gets the fronted
/// platforms through the relay, and a direct adapter of a fronted
/// platform that is not allowed beside it is disconnected.
pub(crate) fn front(shared: &Arc<Shared>, relay: &Arc<Relay>, descriptor: &RelayDescriptor) {
    for profile in shared.server.profiles() {
        front_profile(shared, relay, descriptor, &profile);
    }
}

fn front_profile(
    shared: &Arc<Shared>,
    relay: &Arc<Relay>,
    descriptor: &RelayDescriptor,
    profile: &str,
) {
    for platform in &descriptor.platforms {
        let key = (profile.to_owned(), platform.clone());
        match shared.adapters.slot(profile, platform) {
            Some(slot) if slot.relayed => continue,
            Some(slot) => {
                if !relay.replaces_direct(platform) {
                    continue;
                }
                tracing::info!(
                    profile,
                    platform,
                    "the relay fronts {platform}; disconnecting the direct adapter"
                );
                slot.stop.cancel();
                let adapter = Arc::clone(&slot.adapter);
                tokio::spawn(async move { adapter.disconnect().await });
            }
            None => {
                let direct = shared.agents.scope(profile).is_some_and(|scope| {
                    scope
                        .config()
                        .gateway
                        .platforms
                        .get(platform)
                        .is_some_and(|t| t.enabled)
                });
                if direct && !relay.replaces_direct(platform) {
                    // Allowed beside the relay: the profile's own adapter
                    // serves the platform.
                    continue;
                }
                if direct {
                    tracing::info!(
                        profile,
                        platform,
                        "the relay fronts {platform}; the direct adapter is not connected \
                         (list it in gateway.relay.allow_direct_platforms to keep it)"
                    );
                }
            }
        }
        shared
            .adapters
            .failed
            .write()
            .expect("adapters poisoned")
            .remove(&key);
        let view: Arc<dyn Adapter> = Arc::new(RelayView::new(relay, platform));
        launch_slot(shared, key, view, true);
    }
}

/// Disconnects every adapter.
pub(crate) async fn disconnect_all(shared: &Shared) {
    let slots: Vec<Arc<Slot>> = shared
        .adapters
        .slots
        .read()
        .expect("adapters poisoned")
        .values()
        .cloned()
        .collect();
    for slot in slots {
        slot.stop.cancel();
        slot.adapter.disconnect().await;
        release(&slot);
        slot.set(AdapterState::Disabled, None);
    }
}

/// An adapter's way to the gateway.
struct Link {
    shared: Weak<Shared>,
    profile: String,
    platform: String,
    slot: Weak<Slot>,
}

impl AdapterLink for Link {
    fn submit(&self, inbound: Inbound) -> AdapterFuture<'_, Admission> {
        Box::pin(async move {
            match self.shared.upgrade() {
                Some(shared) => {
                    super::admit::admit(&shared, &self.profile, &self.platform, inbound).await
                }
                None => Admission::Dropped,
            }
        })
    }

    fn report(&self, state: AdapterState, error: Option<AdapterError>) {
        let Some(slot) = self.slot.upgrade() else {
            return;
        };
        slot.set(state, error.as_ref());
        if matches!(state, AdapterState::Retrying | AdapterState::Fatal) {
            slot.lost.notify_one();
        }
    }

    fn deliver<'a>(
        &'a self,
        target: &'a str,
        text: &'a str,
    ) -> AdapterFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let shared = self.shared.upgrade().ok_or("the gateway stopped")?;
            let gateway = super::Gateway { shared };
            let resolved = gateway
                .resolve_target(&self.profile, target)
                .await
                .map_err(|e| e.message().to_owned())?;
            gateway
                .enqueue(&self.profile, &resolved, text, &[])
                .map(|_| resolved.label())
                .map_err(|e| e.message().to_owned())
        })
    }

    fn authorized<'a>(&'a self, sender_id: &'a str) -> AdapterFuture<'a, bool> {
        Box::pin(async move {
            match self.shared.upgrade() {
                Some(shared) => {
                    super::admit::sender_admitted(&shared, &self.profile, &self.platform, sender_id)
                        .await
                }
                None => false,
            }
        })
    }
}

/// The adapters that are connected (or degraded) right now, by
/// `(profile, platform)`.
pub(crate) fn connected(shared: &Shared) -> Vec<((String, String), Arc<Slot>)> {
    shared
        .adapters
        .slots
        .read()
        .expect("adapters poisoned")
        .iter()
        .filter(|(_, slot)| {
            matches!(
                slot.state(),
                AdapterState::Connected | AdapterState::Degraded
            )
        })
        .map(|(key, slot)| (key.clone(), Arc::clone(slot)))
        .collect()
}
