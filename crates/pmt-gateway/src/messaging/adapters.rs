//! Adapter connections (spec: messaging-gateway, Profiles served by one
//! daemon; All adapters down policy; platform-adapters, Adapter state
//! reporting and reconnect): each served profile starts exactly the
//! adapters its own `config.toml` enables; retryable failures reconnect
//! with backoff from 1 second up to 300 seconds; once every adapter has
//! failed for good, `on_all_adapters_down = "exit"` stops the daemon
//! (exit 78) while `stay_alive` keeps serving local clients.

use std::collections::BTreeMap;
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

/// First reconnect delay.
const RECONNECT_START: Duration = Duration::from_secs(1);
/// Longest reconnect delay.
const RECONNECT_MAX: Duration = Duration::from_mins(5);

/// What `gateway status` says about a connected adapter whose send path
/// is not yet confirmed.
pub(crate) const UNCONFIRMED: &str =
    "connected but not yet confirmed active; recovering in background";

/// One running adapter.
pub(crate) struct Slot {
    pub(crate) adapter: Arc<dyn Adapter>,
    pub(crate) descriptor: Descriptor,
    status: Mutex<AdapterStatus>,
    /// The adapter reported a lost connection.
    lost: Notify,
    stop: CancellationToken,
}

impl Slot {
    fn state(&self) -> AdapterState {
        self.status.lock().expect("status poisoned").state
    }

    fn set(&self, state: AdapterState, error: Option<&AdapterError>) {
        let mut status = self.status.lock().expect("status poisoned");
        status.state = state;
        status.code = error.map(|e| e.code.clone());
        status.message = error.map(|e| e.message.clone());
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

    /// The descriptor of a running adapter.
    pub(crate) fn descriptor(&self, profile: &str, platform: &str) -> Option<Descriptor> {
        self.slot(profile, platform).map(|s| s.descriptor.clone())
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
        let sent = slot.adapter.send(message).await?;
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
        slot.adapter.edit(message_id, message).await
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

/// Starts the adapters `profile` enables that are not running yet.
pub(crate) fn start_profile(shared: &Arc<Shared>, profile: &str) {
    let Some(scope) = shared.agents.scope(profile) else {
        return;
    };
    for (platform, table) in &scope.config().gateway.platforms {
        if !table.enabled || shared.adapters.slot(profile, platform).is_some() {
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
    let descriptor = adapter.describe();
    let slot = Arc::new(Slot {
        adapter,
        descriptor,
        status: Mutex::new(AdapterStatus {
            profile: key.0.clone(),
            platform: key.1.clone(),
            state: AdapterState::Connecting,
            code: None,
            message: None,
        }),
        lost: Notify::new(),
        stop: shared.stop.child_token(),
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
        let link: Arc<dyn AdapterLink> = Arc::new(Link {
            shared: Weak::clone(&shared),
            profile: key.0.clone(),
            platform: key.1.clone(),
            slot: Arc::downgrade(&slot),
        });
        slot.set(AdapterState::Connecting, None);
        let outcome = tokio::select! {
            () = slot.stop.cancelled() => return,
            outcome = slot.adapter.connect(link) => outcome,
        };
        match outcome {
            Ok(()) => {
                if slot.state() == AdapterState::Connecting {
                    let unconfirmed = AdapterError {
                        retryable: true,
                        code: "unconfirmed".to_owned(),
                        message: UNCONFIRMED.to_owned(),
                    };
                    slot.set(AdapterState::Degraded, Some(&unconfirmed));
                }
                wait = RECONNECT_START;
                tokio::select! {
                    () = slot.stop.cancelled() => return,
                    () = slot.lost.notified() => {}
                }
            }
            Err(error) if error.retryable => slot.set(AdapterState::Retrying, Some(&error)),
            Err(error) => {
                slot.set(AdapterState::Fatal, Some(&error));
                if let Some(shared) = shared.upgrade() {
                    platform_event(&shared, &key, &error).await;
                    check_all_down(&shared);
                }
                return;
            }
        }
        if slot.state() == AdapterState::Fatal {
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

async fn platform_event(shared: &Shared, key: &(String, String), error: &AdapterError) {
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
}
