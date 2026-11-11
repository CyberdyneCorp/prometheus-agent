//! Browser sessions: resolution, reuse, idle reaping and cleanup (spec:
//! execution-environments, Browser session resolution; Browser
//! supervision, dialogs and cleanup; Local browser options and headed
//! mode; Real-profile browsing; Private URLs route to a local browser).
//!
//! A [`BrowserManager`] holds one live [`BrowserSession`] per session key
//! (the calling session plus an optional `session` name). A key reuses its
//! session while the browser lives; otherwise a new one is created from,
//! in order, an explicit CDP endpoint (`/browser connect` of that chat
//! session, then `browser.cdp_url`), the configured cloud provider, or a
//! local Chromium. A cloud failure falls back to a local browser flagged
//! `fallback_from_cloud`. A cloud session may get a local sidecar for
//! private URLs ([`BrowserManager::sidecar`]).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use pmt_config::schema::BrowserConfig;
use pmt_core::capability::CapabilitySet;
use pmt_net::egress::{HostPolicy, Mode, Proxy, Rules};
use serde_json::{Value, json};
use tokio::time::Instant;

use super::cdp::Cdp;
pub use super::cloud::CloudProvider;
use super::launch::{self, RuntimePolicy};
use super::local::LocalBrowser;
use super::options::{self, Display};
use super::real_profile::{self, Choice, Detect};
use super::records::Records;
use super::safety::UrlSafety;
use super::status::{Entry, Sessions};
use super::supervisor::{Dialogs, Supervisor};
use crate::terminal::env::ChildEnv;

/// Reads the profile's `[browser]` section at use time.
pub type ConfigSource = Arc<dyn Fn() -> BrowserConfig + Send + Sync>;

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
    /// The profile directory (a relative `browser.user_data_dir` resolves
    /// under it).
    pub profile_dir: PathBuf,
    /// Navigation checks.
    pub safety: UrlSafety,
    /// Cloud providers by `browser.provider` name.
    pub providers: BTreeMap<String, Arc<dyn CloudProvider>>,
    /// The OS sandbox local browsers run under.
    pub confinement: launch::Confinement,
    /// The host's display (headed mode).
    pub display: Display,
    /// Finds the user's default browser (real-profile browsing).
    pub detect: Arc<dyn Detect>,
}

/// What one session asks of its browser.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// The session's effective capabilities (`browser.real_profile`).
    pub granted: CapabilitySet,
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

/// A cloud browser to release when the session ends.
struct Lease {
    provider: Arc<dyn CloudProvider>,
    id: String,
}

/// One live browser and its state between calls.
pub struct BrowserSession {
    origin: Origin,
    fallback_from_cloud: bool,
    supervisor: Supervisor,
    local: Option<LocalBrowser>,
    /// What a local browser's profile is (`throwaway`, `user_data_dir`,
    /// `real:<browser>`).
    profile: Option<String>,
    lease: Option<Lease>,
    last_used: Mutex<Instant>,
    /// Fields for the next `browser_exec` result (`notice`,
    /// `real_profile_unavailable`), given once.
    notes: Mutex<Vec<(String, Value)>>,
    /// The local sidecar of a cloud session (private URLs) and whether the
    /// current page is there.
    sidecar: Mutex<Option<Arc<BrowserSession>>>,
    on_sidecar: AtomicBool,
    /// Tabs and the current one.
    pub(super) tabs: tokio::sync::Mutex<super::tabs::Tabs>,
    /// Flattened sessions `browser_cdp` attached, by target or frame id,
    /// with the connection they belong to.
    pub(super) attached: tokio::sync::Mutex<HashMap<String, (String, usize)>>,
    pub(super) safety: UrlSafety,
    status: Mutex<Option<(Sessions, String)>>,
}

impl BrowserSession {
    /// Where the browser runs.
    #[must_use]
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// The cloud provider, for a cloud session.
    #[must_use]
    pub fn provider(&self) -> Option<&str> {
        match &self.origin {
            Origin::Cloud(name) => Some(name),
            _ => None,
        }
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

    /// What a local browser's profile is: `throwaway`, `user_data_dir` or
    /// `real:<browser>`.
    #[must_use]
    pub fn profile(&self) -> Option<&str> {
        self.profile.as_deref()
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

    /// Fields for the next result, once (`notice`,
    /// `real_profile_unavailable`).
    #[must_use]
    pub fn take_notes(&self) -> Vec<(String, Value)> {
        std::mem::take(&mut *self.notes.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn note(&self, field: &str, value: Value) {
        self.notes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((field.to_owned(), value));
    }

    /// The local sidecar, when one was started.
    #[must_use]
    pub fn sidecar(&self) -> Option<Arc<BrowserSession>> {
        self.sidecar
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether the current page is in the sidecar.
    #[must_use]
    pub fn on_sidecar(&self) -> bool {
        self.on_sidecar.load(Ordering::SeqCst)
    }

    /// Makes the sidecar (`true`) or this browser current.
    pub fn use_sidecar(&self, sidecar: bool) {
        self.on_sidecar.store(sidecar, Ordering::SeqCst);
    }

    /// Whether the browser still answers (a dead local process or a
    /// supervisor that gave up means no).
    #[must_use]
    pub fn is_alive(&self) -> bool {
        let process_alive = self.local.as_ref().is_none_or(LocalBrowser::is_running);
        process_alive && !self.supervisor.gave_up()
    }

    /// Kills a local browser and removes its files and releases a cloud
    /// one, with its sidecar; a connected browser is left running.
    async fn terminate(&self) {
        let sidecar = self
            .sidecar
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(sidecar) = sidecar {
            Box::pin(sidecar.terminate()).await;
        }
        if let Some(local) = &self.local {
            local.terminate(&self.cdp()).await;
        }
        if let Some(lease) = &self.lease
            && let Err(why) = lease.provider.release(&lease.id).await
        {
            tracing::warn!(error = %why, "could not release the cloud browser");
        }
        let status = self
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some((sessions, key)) = status {
            sessions.remove(&key);
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
    statuses: Sessions,
    sessions: tokio::sync::Mutex<HashMap<String, Arc<BrowserSession>>>,
    /// `/browser connect` endpoints by chat session id.
    connected: Mutex<HashMap<String, String>>,
    /// The no-display notice was given (once per process).
    told_headless: AtomicBool,
    reaper: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// How often idle sessions are looked for.
const REAP_EVERY: Duration = Duration::from_secs(5);
/// The key suffix of a cloud session's local sidecar.
const SIDECAR: &str = "#local";

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
            statuses: Sessions::new(&setup.cache_dir),
            setup,
            records,
            sessions: tokio::sync::Mutex::default(),
            connected: Mutex::default(),
            told_headless: AtomicBool::new(false),
            reaper: Mutex::default(),
        })
    }

    /// The live `[browser]` configuration.
    #[must_use]
    pub fn config(&self) -> BrowserConfig {
        (self.setup.config)()
    }

    /// The hardening downgrades every local browser starts with now
    /// (`browser.chromium_args` switches `security.sandbox.allow_degraded`
    /// allows); a session reports them like sandbox gaps.
    #[must_use]
    pub fn downgrades(&self) -> Vec<pmt_sandbox::Gap> {
        options::downgrades(
            &self.config(),
            &self.setup.confinement.sandbox.allow_degraded,
        )
    }

    /// Sets (or clears) chat session `session`'s `/browser connect`
    /// endpoint; its browser sessions bound to another endpoint are
    /// replaced on their next use (a cleared one returns to the configured
    /// provider; the connected browser keeps running).
    pub fn set_connect(&self, session: &str, endpoint: Option<String>) {
        let mut connected = self
            .connected
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match endpoint {
            Some(endpoint) => {
                connected.insert(session.to_owned(), endpoint);
            }
            None => {
                connected.remove(session);
            }
        }
    }

    /// Whether chat session `session` ran `/browser connect` (and has not
    /// disconnected since).
    #[must_use]
    pub fn connected(&self, session: &str) -> bool {
        self.connected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(session)
    }

    /// The explicit endpoint session `key` uses: its chat session's
    /// `/browser connect`, else `browser.cdp_url`.
    #[must_use]
    pub fn explicit_endpoint(&self, key: &str) -> Option<String> {
        let session = key.split('/').next().unwrap_or(key);
        let chosen = self
            .connected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(session)
            .cloned();
        chosen.or_else(|| self.config().cdp_url.filter(|url| !url.trim().is_empty()))
    }

    /// The live session of `key`, created when missing or dead.
    ///
    /// # Errors
    ///
    /// No browser could be started or reached.
    pub async fn session(self: &Arc<Self>, key: &str) -> Result<Arc<BrowserSession>, String> {
        self.session_for(key, &Request::default()).await
    }

    /// [`BrowserManager::session`] for a session asking `request`.
    ///
    /// # Errors
    ///
    /// No browser could be started or reached.
    pub async fn session_for(
        self: &Arc<Self>,
        key: &str,
        request: &Request,
    ) -> Result<Arc<BrowserSession>, String> {
        let mut sessions = self.sessions.lock().await;
        let wanted = self.explicit_endpoint(key);
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
        let mut created = self.create(wanted, key, request).await?;
        created.safety.session = Some(key.to_owned());
        self.record_status(key, &created);
        let session = Arc::new(created);
        sessions.insert(key.to_owned(), Arc::clone(&session));
        drop(sessions);
        self.start_reaper();
        Ok(session)
    }

    /// The local sidecar of cloud session `main` (key `key`) for private
    /// URLs or `browser_exec(local=true)`, started when missing or dead.
    ///
    /// # Errors
    ///
    /// No local browser could be started.
    pub async fn sidecar(
        &self,
        main: &BrowserSession,
        key: &str,
        request: &Request,
    ) -> Result<Arc<BrowserSession>, String> {
        if let Some(existing) = main.sidecar().filter(|s| s.is_alive()) {
            existing.touch();
            return Ok(existing);
        }
        let sidecar_key = format!("{key}{SIDECAR}");
        let mut created = self.local(false, &sidecar_key, request).await?;
        created.safety.session = Some(key.to_owned());
        self.record_status(&sidecar_key, &created);
        let sidecar = Arc::new(created);
        let old = main
            .sidecar
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(Arc::clone(&sidecar));
        if let Some(old) = old {
            old.terminate().await;
        }
        Ok(sidecar)
    }

    fn record_status(&self, key: &str, session: &BrowserSession) {
        let (origin, provider, endpoint) = match &session.origin {
            Origin::Connected(endpoint) => ("connected", None, Some(endpoint.clone())),
            Origin::Cloud(provider) => ("cloud", Some(provider.clone()), None),
            Origin::Local => ("local", None, None),
        };
        let config = self.config();
        self.statuses.write(Entry {
            key: key.to_owned(),
            origin: origin.to_owned(),
            provider,
            endpoint,
            pid: session.pid(),
            profile: session.profile.clone(),
            headed: session.local.is_some() && config.headed && self.setup.display.available,
            fallback_from_cloud: session.fallback_from_cloud,
            owner_pid: 0,
            owner_start: 0,
        });
        *session
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner) =
            Some((self.statuses.clone(), key.to_owned()));
    }

    /// Closes the session of `key` (its local browser is killed).
    pub async fn close(&self, key: &str) {
        let closed = self.sessions.lock().await.remove(key);
        if let Some(session) = closed {
            session.terminate().await;
        }
    }

    /// Closes every session whose key starts with `prefix` (one chat
    /// session's browsers, keyed `<session id>/<name>`) and forgets the
    /// chat session's `/browser connect`.
    pub async fn close_prefixed(&self, prefix: &str) {
        self.close_where(|key, _| key.starts_with(prefix)).await;
        self.connected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(prefix.trim_end_matches('/'));
    }

    /// Closes the sessions `pick` chooses.
    async fn close_where(&self, pick: impl Fn(&str, &BrowserSession) -> bool) {
        let closed: Vec<_> = {
            let mut sessions = self.sessions.lock().await;
            let keys: Vec<String> = sessions
                .iter()
                .filter(|(key, session)| pick(key, session))
                .map(|(key, _)| key.clone())
                .collect();
            keys.iter().filter_map(|key| sessions.remove(key)).collect()
        };
        for session in closed {
            session.terminate().await;
        }
    }

    /// `browser_close_profile` for chat session `session`: ends the user's
    /// real browser (`real_profile::close_profile`) so its profile can be
    /// copied, then closes the chat session's local browsers so their next
    /// use launches on a fresh copy.
    ///
    /// # Errors
    ///
    /// `browser.real_profile_autoclose` (or `use_real_profile`) is off, or
    /// the default browser cannot be found.
    pub async fn close_real_profile(&self, session: &str) -> Result<String, String> {
        let config = self.config();
        if !(config.use_real_profile && config.real_profile_autoclose) {
            return Err(
                "browser.real_profile_autoclose is off: ending the user's browser is not offered"
                    .to_owned(),
            );
        }
        let detect = Arc::clone(&self.setup.detect);
        let (name, ended) = pmt_core::spawn::blocking(move || {
            let name = detect.default_browser()?.name;
            real_profile::close_profile(detect.as_ref()).map(|ended| (name, ended))
        })
        .await
        .map_err(|err| format!("closing the browser failed: {err}"))??;
        let prefix = format!("{session}/");
        self.close_where(|key, browser| {
            key.starts_with(&prefix) && matches!(browser.origin(), Origin::Local)
        })
        .await;
        Ok(format!(
            "ended {ended} {name} processes; the next browser call starts on a fresh copy of the profile"
        ))
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
        let limit = Duration::from_secs(self.config().inactivity_timeout_secs);
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

    async fn create(
        &self,
        explicit: Option<String>,
        key: &str,
        request: &Request,
    ) -> Result<BrowserSession, String> {
        if let Some(endpoint) = explicit {
            let socket = super::connect::websocket_url(&endpoint).await?;
            let cdp = Cdp::connect(&socket).await?;
            return Ok(self.session_over(cdp, Origin::Connected(endpoint), None, false));
        }
        let config = self.config();
        if config.provider == "local" {
            return self.local(false, key, request).await;
        }
        let Some(provider) = self.setup.providers.get(&config.provider) else {
            let why = format!("no browser provider '{}' is installed", config.provider);
            return self.fall_back(&config.provider, why, key, request).await;
        };
        let created = match provider.create().await {
            Ok(created) => created,
            Err(why) => return self.fall_back(&config.provider, why, key, request).await,
        };
        match Cdp::connect(&created.endpoint).await {
            Ok(cdp) => {
                let mut session =
                    self.session_over(cdp, Origin::Cloud(config.provider.clone()), None, false);
                session.lease = Some(Lease {
                    provider: Arc::clone(provider),
                    id: created.id,
                });
                Ok(session)
            }
            Err(why) => {
                let _ = provider.release(&created.id).await;
                self.fall_back(&config.provider, why, key, request).await
            }
        }
    }

    async fn fall_back(
        &self,
        provider: &str,
        cloud_why: String,
        key: &str,
        request: &Request,
    ) -> Result<BrowserSession, String> {
        self.local(true, key, request).await.map_err(|local_why| {
            format!(
                "Cloud provider {provider} failed ({cloud_why}) and local fallback also failed ({local_why})"
            )
        })
    }

    async fn local(
        &self,
        fallback_from_cloud: bool,
        key: &str,
        request: &Request,
    ) -> Result<BrowserSession, String> {
        let config = self.config();
        let (mut launch_options, notice) = options::resolve(
            &config,
            &self.setup.profile_dir,
            &self.setup.confinement.sandbox.allow_degraded,
            &self.setup.display,
        )?;
        let choice = real_profile::prepare(
            &config,
            &request.granted,
            self.setup.detect.as_ref(),
            &self.setup.cache_dir,
        );
        let (executable, profile) = if let Choice::Real(browser, snapshot) = &choice {
            launch_options.user_data_dir = Some(snapshot.user_data_dir.clone());
            launch_options
                .extra_args
                .push(format!("--profile-directory={}", snapshot.profile));
            (browser.executable.clone(), format!("real:{}", browser.name))
        } else {
            let executable = match options::executable(&config) {
                Some(path) => path,
                None => self.setup.runtime.executable("chromium").await?,
            };
            let kind = if launch_options.user_data_dir.is_some() {
                "user_data_dir"
            } else {
                "throwaway"
            };
            (executable, kind.to_owned())
        };
        let proxy = self.egress_proxy(key)?;
        let launched = launch::launch(
            &executable,
            &self.setup.cache_dir.join("profiles"),
            &self.setup.env,
            &proxy,
            &self.setup.confinement,
            &launch_options,
        )
        .await?;
        let record = self
            .records
            .add(
                launched.pid,
                &launched.profile_dir,
                &launched.temp_dir,
                launched.keep_profile,
            )
            .map_err(|err| format!("cannot record the browser: {err}"))?;
        let cdp = Arc::clone(&launched.cdp);
        let local = LocalBrowser::new(launched, record, proxy);
        let mut session = self.session_over(cdp, Origin::Local, Some(local), fallback_from_cloud);
        session.profile = Some(profile);
        if let Some(notice) = notice
            && !self.told_headless.swap(true, Ordering::SeqCst)
        {
            session.note("notice", json!(notice));
        }
        if let Choice::Unavailable(why) = choice {
            // With `real_profile_autoclose` the agent may offer to end the
            // browser holding the profile; the tool asks the user first.
            if config.real_profile_autoclose && why.contains("locked") {
                session.note(
                    "real_profile_close",
                    json!("browser_close_profile can end the user's browser so the profile can be copied; it asks the user first"),
                );
            }
            session.note("real_profile_unavailable", json!(why));
        }
        Ok(session)
    }

    /// The egress proxy of session `key`'s local browser: the session's
    /// egress policy (its refusals reported where the session's go) and the
    /// destination checks, for every connection the page makes (subresources,
    /// redirects, scripts), not only the navigations the tools ask for.
    fn egress_proxy(&self, key: &str) -> Result<Proxy, String> {
        let safety = &self.setup.safety;
        let session = key.split('/').next().unwrap_or(key);
        let found = safety.egress.as_ref().and_then(|lookup| lookup(session));
        let (hosts, observer) = match found {
            Some((hosts, observer)) => (hosts, Some(observer)),
            None => (Arc::new(HostPolicy::new(Mode::Public, Vec::new())), None),
        };
        let mut rules = Rules::new(hosts, safety.destinations.clone(), "browser")
            .with_lookup(Arc::clone(&safety.lookup));
        if let Some(observer) = observer {
            rules = rules.observed(observer);
        }
        Proxy::start(rules).map_err(|err| format!("cannot start the browser's egress proxy: {err}"))
    }

    fn session_over(
        &self,
        cdp: Arc<Cdp>,
        origin: Origin,
        local: Option<LocalBrowser>,
        fallback_from_cloud: bool,
    ) -> BrowserSession {
        let config = self.config();
        BrowserSession {
            origin,
            fallback_from_cloud,
            supervisor: Supervisor::start(cdp, config.dialog_policy, config.dialog_timeout_secs),
            local,
            profile: None,
            lease: None,
            last_used: Mutex::new(Instant::now()),
            notes: Mutex::default(),
            sidecar: Mutex::default(),
            on_sidecar: AtomicBool::new(false),
            tabs: tokio::sync::Mutex::default(),
            attached: tokio::sync::Mutex::default(),
            safety: self.setup.safety.clone(),
            status: Mutex::default(),
        }
    }

    /// Starts the idle reaper once; it holds the manager weakly.
    fn start_reaper(self: &Arc<Self>) {
        let mut reaper = self.reaper.lock().unwrap_or_else(PoisonError::into_inner);
        if reaper.is_some() {
            return;
        }
        let manager: Weak<Self> = Arc::downgrade(self);
        *reaper = Some(pmt_core::spawn::spawn(async move {
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
