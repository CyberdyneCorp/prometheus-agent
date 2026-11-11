//! Native command menus (spec: platform-adapters, Native command menus and
//! slash registration): each adapter registers the profile's gateway
//! commands and user-invocable skills after it connects, and again after a
//! registry change. Skills installed or enabled and quick commands edited
//! while the daemon runs change the registry without telling the gateway
//! (another process may have made the change), so a watcher compares the
//! profile's current commands with the ones last registered and registers
//! again when they differ.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tracing::Instrument as _;

use super::Shared;
use super::adapter::MenuCommand;
use super::adapters::{self, Slot};

/// How often the watcher compares the registry with the registered menus.
const POLL: Duration = Duration::from_secs(2);

/// The commands last registered, by profile.
#[derive(Default)]
pub(crate) struct Menus {
    registered: Mutex<HashMap<String, Vec<MenuCommand>>>,
}

impl Menus {
    fn record(&self, profile: &str, commands: &[MenuCommand]) {
        self.registered
            .lock()
            .expect("menus poisoned")
            .insert(profile.to_owned(), commands.to_vec());
    }

    fn changed(&self, profile: &str, commands: &[MenuCommand]) -> bool {
        self.registered
            .lock()
            .expect("menus poisoned")
            .get(profile)
            .is_some_and(|last| last.as_slice() != commands)
    }
}

/// Registers the profile's commands as the adapter's native menu (after
/// a connect); a failure is logged and never fails the connection.
pub(crate) fn register(shared: &Weak<Shared>, key: &(String, String), slot: &Arc<Slot>) {
    let Some(shared) = shared.upgrade() else {
        return;
    };
    let commands = shared.agents.menu_commands(&key.0);
    send(&shared, key, slot, commands);
}

fn send(shared: &Shared, key: &(String, String), slot: &Arc<Slot>, commands: Vec<MenuCommand>) {
    shared.menus.record(&key.0, &commands);
    let adapter = Arc::clone(&slot.adapter);
    let platform = key.1.clone();
    pmt_core::spawn::spawn(
        async move {
            if let Err(err) = adapter.register_commands(&commands).await {
                tracing::warn!(
                    platform,
                    "the command menu was not registered: {}",
                    err.describe()
                );
            }
        }
        .in_current_span(),
    );
}

/// Registers the menus of every connected adapter again (`gateway
/// reload`).
pub(crate) fn refresh(shared: &Arc<Shared>) {
    let weak = Arc::downgrade(shared);
    for (key, slot) in adapters::connected(shared) {
        register(&weak, &key, &slot);
    }
}

/// Starts the watcher that registers a profile's menus again once its
/// commands changed.
pub(crate) fn spawn(shared: &Arc<Shared>) {
    let span = tracing::info_span!("command_menus");
    pmt_core::spawn::spawn(watch(Arc::clone(shared)).instrument(span));
}

async fn watch(shared: Arc<Shared>) {
    loop {
        tokio::select! {
            () = shared.stop.cancelled() => return,
            () = tokio::time::sleep(POLL) => {}
        }
        let connected = adapters::connected(&shared);
        let profiles: BTreeSet<String> = connected.iter().map(|(key, _)| key.0.clone()).collect();
        for profile in profiles {
            // The registry is read from disk (skills, config).
            let agents = Arc::clone(&shared.agents);
            let name = profile.clone();
            let Ok(commands) = pmt_core::spawn::blocking(move || agents.menu_commands(&name)).await
            else {
                continue;
            };
            if !shared.menus.changed(&profile, &commands) {
                continue;
            }
            tracing::info!(
                profile,
                "the command registry changed; registering the menus again"
            );
            for (key, slot) in connected.iter().filter(|(key, _)| key.0 == profile) {
                send(&shared, key, slot, commands.clone());
            }
        }
    }
}
