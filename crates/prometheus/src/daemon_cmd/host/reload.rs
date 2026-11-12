//! Live configuration reload in the daemon (spec: configuration-and-
//! profiles, Live reload with apply classes). Every served profile's
//! `config.toml` is polled; a changed file is validated before use. An
//! invalid one keeps the previous snapshot (logged at ERROR and shown by
//! `daemon status` and `doctor`). A valid one becomes the snapshot new
//! sessions start with (`next_session` keys), is pushed into every running
//! session for its `live` keys (the session's start snapshot with the new
//! live values, through its own overrides: what its turns, tools and
//! approval gate read from then on), and leaves `restart` keys at their
//! old values, listed as pending until the daemon restarts. A running
//! session keeps the snapshot it started with for everything else.

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
    /// The `--set` pins holding `restart` keys at their start values.
    pins: Vec<String>,
}

impl State {
    pub(super) fn new(scope: &ProfileScope) -> Self {
        Self {
            last: std::fs::read(config_path(scope)).ok(),
            ..Self::default()
        }
    }

    /// Why the last reload of the file failed, while it does.
    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The `--set` pins holding `restart` keys at their start values.
    pub(super) fn pins(&self) -> Vec<String> {
        self.pins.clone()
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
        for (_, profile) in host.all_profiles() {
            host.check(&profile).await;
        }
    }
}

impl DaemonHost {
    /// Each served profile's configuration state, for `daemon status`.
    pub(super) fn config_states(&self) -> Vec<ProfileConfig> {
        self.all_profiles()
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

    /// Re-reads `profile`'s `config.toml` when it changed since the last
    /// read (the poll calls it; so does a plugin reload, which must see a
    /// change the CLI wrote a moment ago).
    pub(super) async fn check(&self, profile: &Profile) {
        // The poll may have taken the new bytes and still be applying
        // them: without this a reload asked for meanwhile saw the file as
        // unchanged and loaded the plugins from the old configuration.
        let _rereading = profile.rereading.lock().await;
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
        let launch = self.launch_of(&name);
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
        let (new, pins) = self.pinned(&found, new, launch);
        {
            let mut state = profile.config.lock().expect("config state poisoned");
            state.error = None;
            if found.is_empty() {
                return;
            }
            state.reloads += 1;
            state.pins.extend(pins);
            state.pending_restart.extend(
                found
                    .iter()
                    .filter(|c| c.apply == ApplyClass::Restart)
                    .map(|c| c.key.clone()),
            );
        }
        profile.current.replace(new.clone());
        if found
            .iter()
            .any(|c| c.key == "mcp" || c.key.starts_with("mcp."))
        {
            // MCP servers follow `mcp.servers` (spec: plugin-system, MCP
            // config watch, recycling and discovery wait).
            crate::mcp_host::config_changed(&new);
        }
        // New sessions are built from the new snapshot.
        profile
            .factories
            .lock()
            .expect("factories poisoned")
            .clear();
        if found.iter().any(|c| c.apply == ApplyClass::Live) {
            for session in profile.loaded.lock().await.values() {
                if let Some(effective) =
                    self.effective(profile, &session.setup, &session.live.scope)
                {
                    session
                        .live
                        .gate
                        .set_approvals(effective.config().approvals.clone());
                    session.live.config.replace(effective);
                }
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
    /// after a restart) and the pins that hold them; `new` itself when
    /// nothing needs holding or the held values do not load.
    fn pinned(
        &self,
        found: &[Change],
        new: ProfileScope,
        launch: Launch<'_>,
    ) -> (ProfileScope, Vec<String>) {
        let (pins, unpinnable) = pin_restart_keys(found);
        if !unpinnable.is_empty() {
            tracing::warn!(keys = ?unpinnable, "restart keys that were unset apply to new sessions at once");
        }
        if pins.is_empty() {
            return (new, pins);
        }
        let mut set = self.set.clone();
        set.extend(pins.iter().cloned());
        match Overrides::parse(&set)
            .and_then(|overrides| ProfileScope::open(new.profile().clone(), &overrides, launch))
        {
            Ok(pinned) => (pinned, pins),
            Err(_) => (new, Vec::new()),
        }
    }
}
