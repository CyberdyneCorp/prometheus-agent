//! Browser sessions: resolution, reuse, idle reaping and cleanup (spec:
//! execution-environments, Browser session resolution; Browser
//! supervision, dialogs and cleanup).
//!
//! A [`BrowserManager`] holds one live [`BrowserSession`] per session key
//! (the calling session plus an optional `session` name). A key reuses its
//! session while the browser lives; otherwise a new one is created from,
//! in order, an explicit CDP endpoint (`/browser connect`, then
//! `browser.cdp_url`), the configured cloud provider, or a local Chromium.
//! A cloud failure falls back to a local browser flagged
//! `fallback_from_cloud`.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use pmt_config::schema::BrowserConfig;
use tokio::process::Child;
use tokio::time::Instant;

use super::cdp::Cdp;
use super::launch::{self, RuntimePolicy};
use super::records::Records;
use super::safety::UrlSafety;
use super::supervisor::{Dialogs, Supervisor};
use crate::terminal::env::ChildEnv;

/// Reads the profile's `[browser]` section at use time.
pub type ConfigSource = Arc<dyn Fn() -> BrowserConfig + Send + Sync>;

/// What a cloud provider call returns.
pub type CreateFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>;

/// A cloud browser provider plugin (`browser.provider`): creates a remote
/// browser and returns its DevTools WebSocket endpoint.
pub trait CloudProvider: Send + Sync {
    /// Creates a browser session.
    fn create(&self) -> CreateFuture<'_>;
}

/// Everything a [`BrowserManager`] is built from.
pub struct BrowserSetup {
    /// The live `[browser]` config.
    pub config: ConfigSource,
    /// Where the Chromium executable comes from.
    pub runtime: RuntimePolicy,
    /// The browser process environment.
    pub env: ChildEnv,
    /// `<profile>/cache/browser`.
    pub cache_dir: PathBuf,
    /// Navigation checks.
    pub safety: UrlSafety,
    /// Cloud providers by `browser.provider` name (plugins).
    pub providers: BTreeMap<String, Arc<dyn CloudProvider>>,
}

/// Where a session's browser runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// An explicit CDP endpoint.
    Connected(String),
    /// A cloud provider.
    Cloud(String),
    /// A local Chromium.
    Local,
}

/// A local browser process this session owns.
struct LocalBrowser {
    child: Mutex<Option<Child>>,
    record: PathBuf,
    profile_dir: PathBuf,
    temp_dir: PathBuf,
    pid: u32,
}

impl LocalBrowser {
    fn take_child(&self) -> Option<Child> {
        self.child
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    fn remove_files(&self) {
        let _ = std::fs::remove_file(&self.record);
        let _ = std::fs::remove_dir_all(&self.profile_dir);
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }

    /// Kills the browser, waits for it to exit (a dying browser still
    /// writes to its profile directory) and removes its files.
    async fn terminate(&self) {
        if let Some(mut child) = self.take_child() {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(EXIT_WAIT, child.wait()).await;
        }
        self.remove_files();
    }
}

/// How long a killed browser may take to exit before its files go anyway.
const EXIT_WAIT: Duration = Duration::from_secs(5);

impl Drop for LocalBrowser {
    /// Best effort when a session is dropped without
    /// [`BrowserManager::shutdown`]: kill now, clean up once it exited.
    fn drop(&mut self) {
        let Some(mut child) = self.take_child() else {
            self.remove_files();
            return;
        };
        let _ = child.start_kill();
        let files = (
            std::mem::take(&mut self.record),
            std::mem::take(&mut self.profile_dir),
            std::mem::take(&mut self.temp_dir),
        );
        let cleanup = move || {
            let _ = std::fs::remove_file(&files.0);
            let _ = std::fs::remove_dir_all(&files.1);
            let _ = std::fs::remove_dir_all(&files.2);
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    let _ = tokio::time::timeout(EXIT_WAIT, child.wait()).await;
                    cleanup();
                });
            }
            Err(_) => cleanup(),
        }
    }
}

/// One live browser and its state between calls.
pub struct BrowserSession {
    origin: Origin,
    fallback_from_cloud: bool,
    supervisor: Supervisor,
    local: Option<LocalBrowser>,
    last_used: Mutex<Instant>,
    /// Tabs and the current one.
    pub(super) tabs: tokio::sync::Mutex<super::tabs::Tabs>,
    pub(super) safety: UrlSafety,
}

impl BrowserSession {
    /// Where the browser runs.
    #[must_use]
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// The session replaced a failed cloud browser with a local one.
    #[must_use]
    pub fn fallback_from_cloud(&self) -> bool {
        self.fallback_from_cloud
    }

    /// The local browser's pid.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.local.as_ref().map(|local| local.pid)
    }

    /// The current DevTools connection.
    #[must_use]
    pub fn cdp(&self) -> Arc<Cdp> {
        self.supervisor.cdp()
    }

    /// Pending and handled JavaScript dialogs.
    #[must_use]
    pub fn dialogs(&self) -> &Arc<Dialogs> {
        self.supervisor.dialogs()
    }

    /// Whether the browser still answers (a dead local process or a
    /// supervisor that gave up means no).
    #[must_use]
    pub fn is_alive(&self) -> bool {
        let process_alive = self.local.as_ref().is_none_or(|local| {
            matches!(
                local
                    .child
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_mut()
                    .map(Child::try_wait),
                Some(Ok(None))
            )
        });
        process_alive && !self.supervisor.gave_up()
    }

    /// Kills a local browser and removes its files (a connected or cloud
    /// browser is left running).
    async fn terminate(&self) {
        if let Some(local) = &self.local {
            local.terminate().await;
        }
    }

    fn touch(&self) {
        *self
            .last_used
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last_used
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .elapsed()
    }
}

/// The browser sessions of one profile in this process.
pub struct BrowserManager {
    setup: BrowserSetup,
    records: Records,
    sessions: tokio::sync::Mutex<HashMap<String, Arc<BrowserSession>>>,
    connect_override: Mutex<Option<String>>,
    reaper: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// How often idle sessions are looked for.
const REAP_EVERY: Duration = Duration::from_secs(5);

impl BrowserManager {
    /// A manager over `setup`; browsers left by crashed owners of this
    /// profile are reaped first.
    #[must_use]
    pub fn new(setup: BrowserSetup) -> Arc<Self> {
        let records = Records::new(&setup.cache_dir);
        let reaped = records.reap_orphans();
        if reaped > 0 {
            tracing::info!(reaped, "reaped browsers of crashed owners");
        }
        Arc::new(Self {
            setup,
            records,
            sessions: tokio::sync::Mutex::default(),
            connect_override: Mutex::default(),
            reaper: Mutex::default(),
        })
    }

    /// Sets (or clears) the `/browser connect` endpoint; sessions bound to
    /// another endpoint are replaced on their next use.
    pub fn set_connect(&self, endpoint: Option<String>) {
        *self
            .connect_override
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = endpoint;
    }

    /// The live session of `key`, created when missing or dead.
    ///
    /// # Errors
    ///
    /// No browser could be started or reached.
    pub async fn session(self: &Arc<Self>, key: &str) -> Result<Arc<BrowserSession>, String> {
        let mut sessions = self.sessions.lock().await;
        let wanted = self.explicit_endpoint();
        if let Some(existing) = sessions.get(key) {
            let rebound = match (&existing.origin, &wanted) {
                (Origin::Connected(bound), Some(endpoint)) => bound != endpoint,
                (Origin::Connected(_), None) | (_, Some(_)) => true,
                _ => false,
            };
            if existing.is_alive() && !rebound {
                existing.touch();
                return Ok(Arc::clone(existing));
            }
            if let Some(old) = sessions.remove(key) {
                old.terminate().await;
            }
        }
        let session = Arc::new(self.create(wanted).await?);
        sessions.insert(key.to_owned(), Arc::clone(&session));
        drop(sessions);
        self.start_reaper();
        Ok(session)
    }

    /// Closes the session of `key` (its local browser is killed).
    pub async fn close(&self, key: &str) {
        let closed = self.sessions.lock().await.remove(key);
        if let Some(session) = closed {
            session.terminate().await;
        }
    }

    /// Closes every session (session end): local browsers are killed and
    /// their files removed before this returns.
    pub async fn shutdown(&self) {
        let closed: Vec<_> = self.sessions.lock().await.drain().map(|(_, s)| s).collect();
        for session in closed {
            session.terminate().await;
        }
    }

    /// Closes every session idle longer than `browser.inactivity_timeout_secs`
    /// (read now) or whose browser died; returns how many were closed.
    pub async fn reap_idle(&self) -> usize {
        let limit = Duration::from_secs((self.setup.config)().inactivity_timeout_secs);
        let closed: Vec<_> = {
            let mut sessions = self.sessions.lock().await;
            let stale: Vec<String> = sessions
                .iter()
                .filter(|(_, s)| s.idle_for() > limit || !s.is_alive())
                .map(|(key, _)| key.clone())
                .collect();
            stale
                .iter()
                .filter_map(|key| sessions.remove(key))
                .collect()
        };
        for session in &closed {
            session.terminate().await;
        }
        closed.len()
    }

    fn explicit_endpoint(&self) -> Option<String> {
        let chosen = self
            .connect_override
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        chosen.or_else(|| (self.setup.config)().cdp_url)
    }

    async fn create(&self, explicit: Option<String>) -> Result<BrowserSession, String> {
        if let Some(endpoint) = explicit {
            let cdp = Cdp::connect(&endpoint).await?;
            return Ok(self.session_over(cdp, Origin::Connected(endpoint), None, false));
        }
        let config = (self.setup.config)();
        if config.provider == "local" {
            return self.local(false).await;
        }
        let cloud = match self.setup.providers.get(&config.provider) {
            Some(provider) => match provider.create().await {
                Ok(endpoint) => Cdp::connect(&endpoint).await,
                Err(why) => Err(why),
            },
            None => Err(format!(
                "no browser provider '{}' is installed",
                config.provider
            )),
        };
        match cloud {
            Ok(cdp) => Ok(self.session_over(cdp, Origin::Cloud(config.provider), None, false)),
            Err(cloud_why) => self.local(true).await.map_err(|local_why| {
                format!(
                    "Cloud provider {} failed ({cloud_why}) and local fallback also failed ({local_why})",
                    config.provider
                )
            }),
        }
    }

    async fn local(&self, fallback_from_cloud: bool) -> Result<BrowserSession, String> {
        let executable = self.setup.runtime.executable("chromium").await?;
        let launched = launch::launch(
            &executable,
            &self.setup.cache_dir.join("profiles"),
            &self.setup.env,
            launch::LAUNCH_TIMEOUT,
        )
        .await?;
        let record = self
            .records
            .add(launched.pid, &launched.profile_dir, &launched.temp_dir)
            .map_err(|err| format!("cannot record the browser: {err}"))?;
        let local = LocalBrowser {
            child: Mutex::new(Some(launched.child)),
            record,
            profile_dir: launched.profile_dir,
            temp_dir: launched.temp_dir,
            pid: launched.pid,
        };
        let cdp = Cdp::connect(&launched.endpoint).await?;
        Ok(self.session_over(cdp, Origin::Local, Some(local), fallback_from_cloud))
    }

    fn session_over(
        &self,
        cdp: Arc<Cdp>,
        origin: Origin,
        local: Option<LocalBrowser>,
        fallback_from_cloud: bool,
    ) -> BrowserSession {
        let config = (self.setup.config)();
        BrowserSession {
            origin,
            fallback_from_cloud,
            supervisor: Supervisor::start(cdp, config.dialog_policy, config.dialog_timeout_secs),
            local,
            last_used: Mutex::new(Instant::now()),
            tabs: tokio::sync::Mutex::default(),
            safety: self.setup.safety.clone(),
        }
    }

    /// Starts the idle reaper once; it holds the manager weakly.
    fn start_reaper(self: &Arc<Self>) {
        let mut reaper = self.reaper.lock().unwrap_or_else(PoisonError::into_inner);
        if reaper.is_some() {
            return;
        }
        let manager: Weak<Self> = Arc::downgrade(self);
        *reaper = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(REAP_EVERY).await;
                let Some(manager) = manager.upgrade() else {
                    return;
                };
                manager.reap_idle().await;
            }
        }));
    }
}

impl Drop for BrowserManager {
    fn drop(&mut self) {
        if let Some(reaper) = self
            .reaper
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            reaper.abort();
        }
    }
}
