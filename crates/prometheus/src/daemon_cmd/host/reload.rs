//! Live configuration reload in the daemon (spec: configuration-and-
//! profiles, Live reload with apply classes). Every served profile's
//! `config.toml` is polled; a changed file is validated before use. An
//! invalid one keeps the previous snapshot (logged at ERROR and shown by
//! `daemon status` and `doctor`). A valid one becomes the snapshot new
//! sessions start with (`next_session` keys), is pushed into running
//! sessions for the `live` keys they read at run time (the approval
//! settings; the daemon's own budget and resume checks always read the
//! current snapshot), and leaves `restart` keys at their old values,
//! listed as pending until the daemon restarts. A running session keeps
//! the snapshot it started with for everything else.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use pmt_config::catalog::ApplyClass;
use pmt_config::reload::{Change, changes, pin_restart_keys};
use pmt_config::{Launch, Overrides, ProfileScope};
use pmt_rpc::control::ProfileConfig;

use super::{DaemonHost, Profile};

/// How often the files are compared with what was loaded.
pub(in crate::daemon_cmd) const POLL: Duration = Duration::from_millis(500);

/// One profile's reload state.
#[derive(Debug, Default)]
pub(super) struct State {
    /// The file's bytes as last read.
    last: Option<Vec<u8>>,
    reloads: u64,
    error: Option<String>,
    pending_restart: BTreeSet<String>,
}

impl State {
    pub(super) fn new(scope: &ProfileScope) -> Self {
        Self {
            last: std::fs::read(config_path(scope)).ok(),
            ..Self::default()
        }
    }
}

fn config_path(scope: &ProfileScope) -> std::path::PathBuf {
    scope.path("config.toml")
}

/// Polls every served profile's `config.toml` until the daemon stops.
pub(in crate::daemon_cmd) async fn watch(host: Arc<DaemonHost>) {
    let mut tick = tokio::time::interval(POLL);
    loop {
        tick.tick().await;
        for profile in host.profiles.values() {
            host.check(profile).await;
        }
    }
}

impl DaemonHost {
    /// Each served profile's configuration state, for `daemon status`.
    pub(super) fn config_states(&self) -> Vec<ProfileConfig> {
        self.profiles
            .iter()
            .map(|(name, profile)| {
                let state = profile.config.lock().expect("config state poisoned");
                ProfileConfig {
                    profile: name.clone(),
                    reloads: state.reloads,
                    error: state.error.clone(),
                    pending_restart: state.pending_restart.iter().cloned().collect(),
                }
            })
            .collect()
    }

    async fn check(&self, profile: &Profile) {
        let current = profile.scope();
        let text = std::fs::read(config_path(&current)).ok();
        {
            let mut state = profile.config.lock().expect("config state poisoned");
            if state.last == text {
                return;
            }
            state.last = text;
        }
        let name = current.name().to_string();
        let launch = if name == self.launch_profile {
            Launch::Profile(&self.launch)
        } else {
            Launch::Other
        };
        let opened = Overrides::parse(&self.set).and_then(|overrides| {
            ProfileScope::open(current.profile().clone(), &overrides, launch)
        });
        match opened {
            Ok(new) => self.apply(profile, &current, new, launch).await,
            Err(err) => {
                tracing::error!(
                    profile = name.as_str(),
                    "config.toml is invalid; still serving the previous configuration: {err}"
                );
                profile.config.lock().expect("config state poisoned").error = Some(format!(
                    "{}: {}",
                    config_path(&current).display(),
                    err.message()
                ));
            }
        }
    }

    async fn apply(
        &self,
        profile: &Profile,
        old: &ProfileScope,
        new: ProfileScope,
        launch: Launch<'_>,
    ) {
        let found = changes(old.config(), new.config());
        let new = self.pinned(&found, new, launch);
        {
            let mut state = profile.config.lock().expect("config state poisoned");
            state.error = None;
            if found.is_empty() {
                return;
            }
            state.reloads += 1;
            state.pending_restart.extend(
                found
                    .iter()
                    .filter(|c| c.apply == ApplyClass::Restart)
                    .map(|c| c.key.clone()),
            );
        }
        *profile
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = new.clone();
        // New sessions are built from the new snapshot.
        profile
            .factories
            .lock()
            .expect("factories poisoned")
            .clear();
        if found
            .iter()
            .any(|c| c.apply == ApplyClass::Live && c.key.starts_with("approvals."))
        {
            for session in profile.loaded.lock().await.values() {
                session
                    .live
                    .gate
                    .set_approvals(new.config().approvals.clone());
            }
        }
        let keys = |class: ApplyClass| -> Vec<&str> {
            found
                .iter()
                .filter(|c| c.apply == class)
                .map(|c| c.key.as_str())
                .collect()
        };
        tracing::info!(
            profile = new.name().as_str(),
            live = ?keys(ApplyClass::Live),
            next_session = ?keys(ApplyClass::NextSession),
            restart = ?keys(ApplyClass::Restart),
            "config.toml reloaded"
        );
    }

    /// `new` with its `restart` keys held at their old values (they apply
    /// after a restart); `new` itself when nothing needs holding or the
    /// held values do not load.
    fn pinned(&self, found: &[Change], new: ProfileScope, launch: Launch<'_>) -> ProfileScope {
        let (pins, unpinnable) = pin_restart_keys(found);
        if !unpinnable.is_empty() {
            tracing::warn!(keys = ?unpinnable, "restart keys that were unset apply to new sessions at once");
        }
        if pins.is_empty() {
            return new;
        }
        let mut set = self.set.clone();
        set.extend(pins);
        Overrides::parse(&set)
            .and_then(|overrides| ProfileScope::open(new.profile().clone(), &overrides, launch))
            .unwrap_or(new)
    }
}
