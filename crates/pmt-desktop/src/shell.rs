//! The shell: what the webview's one entry point does with each
//! [`Request`]. It attaches to the daemon through the bundled binary,
//! prepares dial tickets and runs the allowlisted native commands; the
//! Tauri app and the desktop browser tests both drive it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pmt_rpc::control::{self, ControlRequest};
use pmt_rpc::discovery::{self, Discovery};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::OnceCell;

use crate::attention::Notices;
use crate::deeplink::DeepLink;
use crate::dial::{Dial, DialPurpose, Dials};
use crate::endpoint::{self, LocalEndpoint};
use crate::ensure::{self, BinaryIdentity, EnsureReport, RunError};
use crate::links::{self, Opened, Target};
use crate::options;
use crate::panels::Panels;
use crate::quick_ask::QuickAsks;
use crate::request::{Native, Request};
use crate::skew::{self, Skew};
use crate::update::Updater;

/// Where the shell finds things.
#[derive(Debug, Clone)]
pub struct ShellConfig {
    /// The bundled `prometheus` binary.
    pub binary: PathBuf,
    /// The Prometheus home (for the daemon log link).
    pub home: PathBuf,
    /// The user's home directory (the guarded opener's root).
    pub user_home: PathBuf,
    /// The bound of `daemon ensure` (60 s plus 15 s in the app).
    pub ensure_bound: Duration,
}

/// The outcome of attaching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Attach {
    /// Attached to a validated local daemon.
    Attached {
        /// The daemon's version.
        version: String,
        /// The profiles it serves.
        profiles: Vec<String>,
        /// A notice to show (a daemon newer than the app).
        #[serde(skip_serializing_if = "Option::is_none")]
        notice: Option<String>,
    },
    /// A stale daemon has running turns: ask before restarting it.
    ConfirmRestart {
        /// Turns running now.
        running_turns: usize,
        /// The daemon's version.
        daemon: String,
        /// The bundled version.
        bundled: String,
    },
    /// Started with `--ignore-existing`: no local daemon is ensured.
    NoLocal,
    /// Not attached.
    Failed {
        /// What went wrong.
        error: String,
        /// The daemon log to look at, for a timeout.
        #[serde(skip_serializing_if = "Option::is_none")]
        log: Option<String>,
    },
}

impl Attach {
    fn failed(error: &impl ToString) -> Self {
        Self::Failed {
            error: error.to_string(),
            log: None,
        }
    }
}

/// The desktop shell.
pub struct Shell {
    pub(crate) config: ShellConfig,
    pub(crate) native: Arc<dyn Native>,
    pub(crate) dials: Dials,
    endpoint: Mutex<Option<LocalEndpoint>>,
    bundled: OnceCell<BinaryIdentity>,
    pub(crate) updater: Updater,
    pub(crate) options: options::Read,
    pub(crate) notices: Notices,
    pub(crate) quick_asks: QuickAsks,
    pub(crate) panels: Panels,
    pub(crate) links: Mutex<Links>,
    pub(crate) hotkey: Mutex<Option<crate::features::Hotkey>>,
    /// The HUD snap chord's registration.
    pub(crate) hud_snap: Mutex<Option<crate::features::Hotkey>>,
    pub(crate) connections: crate::connections::service::Connections,
    local: bool,
}

/// Deep links that arrived before the window asked for them.
#[derive(Debug, Default)]
pub(crate) struct Links {
    pub(crate) ready: bool,
    pub(crate) pending: Vec<DeepLink>,
}

impl Shell {
    /// A shell over `native`.
    #[must_use]
    pub fn new(config: ShellConfig, native: Arc<dyn Native>) -> Self {
        Self {
            config,
            native,
            dials: Dials::default(),
            endpoint: Mutex::new(None),
            bundled: OnceCell::new(),
            updater: Updater::disabled(),
            options: options::Read::default(),
            notices: Notices::default(),
            quick_asks: QuickAsks::default(),
            panels: Panels::default(),
            links: Mutex::new(Links::default()),
            hotkey: Mutex::new(None),
            hud_snap: Mutex::new(None),
            connections: crate::connections::service::Connections::default(),
            local: true,
        }
    }

    /// With the release build's updater.
    #[must_use]
    pub fn with_updater(mut self, updater: Updater) -> Self {
        self.updater = updater;
        self
    }

    /// With the `[desktop]` options read at launch.
    #[must_use]
    pub fn with_options(mut self, read: options::Read) -> Self {
        self.options = read;
        self
    }

    /// Started with `--ignore-existing`: attaching ensures no local daemon.
    #[must_use]
    pub fn without_local_daemon(mut self) -> Self {
        self.local = false;
        self
    }

    /// The updater.
    #[must_use]
    pub fn updater(&self) -> &Updater {
        &self.updater
    }

    /// The options read at launch.
    #[must_use]
    pub fn options(&self) -> &options::DesktopOptions {
        &self.options.options
    }

    /// The approval notifications.
    #[must_use]
    pub fn notices(&self) -> &Notices {
        &self.notices
    }

    /// The plugin panels served.
    #[must_use]
    pub fn panels(&self) -> &Panels {
        &self.panels
    }

    /// A deep link arrived: kept for the window until it is ready (then
    /// `false`); `true` once it is, and the caller sends it as an event.
    ///
    /// # Panics
    ///
    /// When the links lock was poisoned by a panic elsewhere.
    pub fn arrived(&self, link: DeepLink) -> bool {
        let mut links = self.links.lock().expect("links poisoned");
        if links.ready {
            return true;
        }
        links.pending.push(link);
        false
    }

    /// Handles one request of `window`.
    ///
    /// # Errors
    ///
    /// A message for the window (the request did not happen).
    pub async fn handle(&self, window: &str, request: Request) -> Result<Value, String> {
        match request {
            Request::Attach { profile } => to_json(&self.attach(profile.as_deref(), false).await),
            Request::RestartDaemon { profile } => {
                to_json(&self.attach(profile.as_deref(), true).await)
            }
            Request::PrepareDial {
                connection,
                profile,
                path,
                purpose,
            } if !Self::is_local(connection.as_deref()) => {
                let id = connection.unwrap_or_default();
                to_json(
                    &self
                        .dial_connection(window, &id, &profile, &path, purpose)
                        .await?,
                )
            }
            Request::PrepareDial {
                profile,
                path,
                purpose,
                ..
            } => to_json(&self.prepare_dial(window, &profile, &path, purpose).await?),
            Request::SettleDial { dial } => {
                self.dials
                    .settle(window, &dial)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"ok": true}))
            }
            Request::OpenExternal { url, workspace } => {
                to_json(&self.open_external(&url, workspace.as_deref()))
            }
            Request::OpenFile { path, workspace } => {
                to_json(&self.open_file(Path::new(&path), workspace.as_deref()))
            }
            Request::RevealInFolder { path, workspace } => {
                to_json(&self.reveal(Path::new(&path), workspace.as_deref()))
            }
            Request::PickFile { title } => self.pick(false, title).await,
            Request::PickDirectory { title } => self.pick(true, title).await,
            other if crate::features::owns(&other) => self.feature(window, other).await,
            other if crate::profile_files::owns(&other) => self.profile_file(other).await,
            other if crate::connections::service::owns(&other) => {
                self.connection_request(other).await
            }
            other => self.handle_simple(window, other),
        }
    }

    /// The requests that answer at once.
    fn handle_simple(&self, window: &str, request: Request) -> Result<Value, String> {
        match request {
            Request::Notify { title, body } => self.native.notify(&title, &body).map(|()| ok()),
            Request::SetBadge { count } => self.native.set_badge(window, count).map(|()| ok()),
            Request::RegisterHotkey { chord } => self.native.register_hotkey(&chord).map(|()| ok()),
            Request::ReadClipboard => self
                .native
                .read_clipboard()
                .map(|text| json!({"ok": true, "text": text})),
            Request::PluginRefused { plugin, command } => {
                tracing::warn!(
                    plugin = plugin.as_str(),
                    command = command.as_str(),
                    "refused a native command from a plugin frame"
                );
                Ok(json!({"ok": false, "reason": "plugin"}))
            }
            _ => Err("not a request of this kind".to_owned()),
        }
    }

    /// Ensures the daemon, validates its endpoint and checks the skew;
    /// `confirmed` restarts a stale daemon even while turns run.
    ///
    /// # Panics
    ///
    /// When the endpoint lock was poisoned by a panic elsewhere.
    pub async fn attach(&self, profile: Option<&str>, confirmed: bool) -> Attach {
        if !self.local {
            return Attach::NoLocal;
        }
        let first = match self.ensure_endpoint(profile).await {
            Ok(found) => found,
            Err(failed) => return failed,
        };
        let bundled = match self.bundled().await {
            Ok(identity) => identity,
            Err(e) => return Attach::failed(&e),
        };
        let skew = skew::compare(
            (&first.version, &first.build_id),
            (&bundled.version, &bundled.build_id),
        );
        let status = daemon_status(&first.control).await;
        let endpoint = match skew {
            Skew::Stale => {
                let running = status.as_ref().map_or(0, |s| s.active_turns);
                if running > 0 && !confirmed {
                    // Usable as it is until the user agrees to restart it.
                    *self.endpoint.lock().expect("endpoint poisoned") = Some(first.clone());
                    return Attach::ConfirmRestart {
                        running_turns: running,
                        daemon: first.version,
                        bundled: bundled.version.clone(),
                    };
                }
                match self.restart(profile).await {
                    Ok(endpoint) => endpoint,
                    Err(failed) => return failed,
                }
            }
            Skew::Same | Skew::DaemonNewer { .. } => first,
        };
        let notice = match skew::compare(
            (&endpoint.version, &endpoint.build_id),
            (&bundled.version, &bundled.build_id),
        ) {
            Skew::DaemonNewer { daemon, bundled } => Some(skew::update_notice(&daemon, &bundled)),
            _ => None,
        };
        let profiles = daemon_status(&endpoint.control)
            .await
            .map(|s| s.profiles)
            .unwrap_or_default();
        let version = endpoint.version.clone();
        *self.endpoint.lock().expect("endpoint poisoned") = Some(endpoint);
        Attach::Attached {
            version,
            profiles,
            notice,
        }
    }

    async fn ensure_endpoint(&self, profile: Option<&str>) -> Result<LocalEndpoint, Attach> {
        let report = ensure::ensure(&self.config.binary, profile, self.config.ensure_bound)
            .await
            .map_err(|e| self.run_failure(&e))?;
        endpoint::validate_local(&report, pmt_core::protocol::RPC_PROTOCOL.major)
            .map_err(|e| Attach::failed(&e))
    }

    async fn restart(&self, profile: Option<&str>) -> Result<LocalEndpoint, Attach> {
        let mut args = vec!["daemon", "restart"];
        if let Some(profile) = profile {
            args.extend(["--profile", profile]);
        }
        let status = tokio::time::timeout(
            self.config.ensure_bound,
            tokio::process::Command::new(&self.config.binary)
                .args(&args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await;
        if !matches!(status, Ok(Ok(s)) if s.success()) {
            return Err(Attach::failed(
                &"the stale gateway daemon could not be restarted",
            ));
        }
        self.ensure_endpoint(profile).await
    }

    fn run_failure(&self, error: &RunError) -> Attach {
        let log = matches!(error, RunError::TimedOut { .. }).then(|| {
            self.config
                .home
                .join("logs")
                .join("gateway.log")
                .to_string_lossy()
                .into_owned()
        });
        Attach::Failed {
            error: error.to_string(),
            log,
        }
    }

    async fn bundled(&self) -> Result<&BinaryIdentity, RunError> {
        self.bundled
            .get_or_try_init(|| ensure::identity(&self.config.binary, Duration::from_secs(15)))
            .await
    }

    /// A ticket for one dial of `window`, bound to its path and purpose.
    /// Each dial goes to the daemon the home's discovery file names, so a
    /// daemon restarted elsewhere (`daemon restart`, an update) is found
    /// again; a stopped one is not started.
    ///
    /// # Errors
    ///
    /// Not attached, an unknown path, too many pending dials, or the
    /// daemon refused the ticket.
    ///
    /// # Panics
    ///
    /// When the endpoint lock was poisoned by a panic elsewhere.
    pub async fn prepare_dial(
        &self,
        window: &str,
        profile: &str,
        path: &str,
        purpose: DialPurpose,
    ) -> Result<Dial, String> {
        self.current().ok_or("not attached to a gateway daemon")?;
        // The daemon may have restarted on a new port since the last dial
        // (its control channel keeps its path): dial what it published.
        let endpoint = self
            .rediscover()
            .or_else(|| self.current())
            .ok_or("not attached to a gateway daemon")?;
        if path != endpoint::RPC_PATH {
            return Err(format!("unknown gateway path {path}"));
        }
        let id = self
            .dials
            .reserve(window, path, purpose, Instant::now())
            .map_err(|e| e.to_string())?;
        let ticket = ticket_for(&endpoint, profile, purpose).await;
        match ticket {
            Ok(ticket) => Ok(Dial {
                dial: id,
                url: endpoint.rpc_url,
                protocols: vec![
                    format!("prometheus-rpc.v{}", endpoint.protocol_major),
                    format!("prometheus-ticket.{ticket}"),
                ],
            }),
            Err(message) => {
                let _ = self.dials.settle(window, &id);
                Err(message)
            }
        }
    }

    pub(crate) fn current(&self) -> Option<LocalEndpoint> {
        self.endpoint.lock().expect("endpoint poisoned").clone()
    }

    /// The home's running daemon from its discovery file, validated as an
    /// ensure result is; it becomes the shell's endpoint.
    pub(crate) fn rediscover(&self) -> Option<LocalEndpoint> {
        let Ok(Discovery::Running(record)) = discovery::discover(&self.config.home) else {
            return None;
        };
        let e = record.endpoint;
        let report = EnsureReport {
            rpc_url: e.rpc_url,
            control: e.control,
            instance_id: e.instance_id,
            build_id: e.build_id,
            version: record.process.version,
            rpc_protocol: e.rpc_protocol,
            features: e.features,
        };
        let found =
            endpoint::validate_local(&report, pmt_core::protocol::RPC_PROTOCOL.major).ok()?;
        *self.endpoint.lock().expect("endpoint poisoned") = Some(found.clone());
        Some(found)
    }

    /// The external URL command.
    #[must_use]
    pub fn open_external(&self, raw: &str, workspace: Option<&str>) -> Opened {
        match links::classify(raw, &self.config.user_home) {
            Target::Url(url) => opened(self.native.open_url(&url)),
            Target::File(path) => self.open_file(&path, workspace),
            Target::Invalid => Opened::refused("invalid"),
        }
    }

    fn open_file(&self, path: &Path, workspace: Option<&str>) -> Opened {
        match links::guard_file(path, &self.config.user_home, workspace.map(Path::new)) {
            Ok(real) => opened(self.native.open_path(&real)),
            Err(reason) => Opened::refused(reason),
        }
    }

    fn reveal(&self, path: &Path, workspace: Option<&str>) -> Opened {
        match links::guard_inside(path, &self.config.user_home, workspace.map(Path::new)) {
            Ok(real) => opened(self.native.reveal(&real)),
            Err(reason) => Opened::refused(reason),
        }
    }

    async fn pick(&self, directory: bool, title: Option<String>) -> Result<Value, String> {
        let native = Arc::clone(&self.native);
        let picked = pmt_core::spawn::blocking(move || native.pick(directory, title.as_deref()))
            .await
            .map_err(|e| e.to_string())?;
        Ok(json!({"path": picked.map(|p| p.to_string_lossy().into_owned())}))
    }
}

pub(crate) fn ok() -> Value {
    json!({"ok": true})
}

fn opened(result: Result<(), String>) -> Opened {
    match result {
        Ok(()) => Opened::OK,
        Err(why) => {
            tracing::warn!(why, "the OS could not open a link");
            Opened::refused("failed")
        }
    }
}

pub(crate) fn to_json<T: Serialize>(value: &T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|e| e.to_string())
}

/// A one-use ticket of `endpoint`'s daemon for `profile`.
async fn ticket_for(
    endpoint: &LocalEndpoint,
    profile: &str,
    purpose: DialPurpose,
) -> Result<String, String> {
    let request = ControlRequest::Ticket {
        purpose: purpose.into(),
        profiles: vec![profile.to_owned()],
        pid: Some(std::process::id()),
    };
    control::call(&endpoint.control, &request)
        .await
        .and_then(control::ticket_of)
        .map(|t| t.ticket)
        .map_err(|e| e.message().to_owned())
}

pub(crate) async fn daemon_status(control: &str) -> Option<control::DaemonStatus> {
    control::call(control, &ControlRequest::Status)
        .await
        .ok()
        .and_then(|r| r.status)
}
