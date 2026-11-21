//! One session's computer use (spec: computer-use, Permission modes are
//! fixed per runtime; `computer_use` tool actions).
//!
//! The session owns at most one driver runtime. Its permission mode is
//! derived at every action from the session's approval state — `/yolo`
//! gives `unrestricted`, `computer_use.permission_mode = "bounded"` gives
//! `bounded`, anything else `standard` — and a runtime whose mode no
//! longer matches is stopped before the next one starts, so a mode never
//! changes under a live runtime and never leaks to another session (each
//! session launches its own). The session also keeps the sticky target
//! (the window the last capture or `focus_app` chose) and the element
//! indices of the latest capture.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use pmt_config::schema::{ComputerUseConfig, ComputerUsePermission};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use super::driver::{Driver, DriverReply, Launcher, PermissionMode, RuntimeSpec};

/// Whether `/yolo` is on for the session, asked at every action.
pub type YoloProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// What a session's computer use is built from.
pub struct SessionSetup {
    /// The session id.
    pub session_id: String,
    /// The profile's home directory (screenshots, the screen lease).
    pub profile_dir: PathBuf,
    /// `[computer_use]` as the session started.
    pub config: ComputerUseConfig,
    /// The main model takes images (captures carry their screenshot).
    pub image_input: bool,
    /// Whether the agent cursor overlay is shown (decided per platform).
    pub overlay: bool,
    /// Starts the backend's runtimes.
    pub launcher: Arc<dyn Launcher>,
}

/// The window input goes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Its process.
    pub pid: i64,
    /// Its window.
    pub window_id: i64,
    /// Its application name.
    pub app: String,
}

/// A live runtime and the mode it was launched in.
struct Runtime {
    mode: PermissionMode,
    driver: Arc<dyn Driver>,
}

/// The mutable part, behind one lock so calls run one at a time.
#[derive(Default)]
pub(super) struct State {
    runtime: Option<Runtime>,
    /// The sticky target.
    pub(super) target: Option<Target>,
    /// Element index -> element token of the latest capture.
    pub(super) elements: BTreeMap<i64, Option<String>>,
}

/// The state of one call and its runtime.
pub(super) struct Locked<'a> {
    /// The locked state.
    pub(super) state: tokio::sync::MutexGuard<'a, State>,
    /// The runtime.
    pub(super) driver: Arc<dyn Driver>,
}

/// One session's computer use.
pub struct ComputerUseSession {
    setup: SessionSetup,
    yolo: OnceLock<YoloProbe>,
    state: Mutex<State>,
}

impl std::fmt::Debug for ComputerUseSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComputerUseSession")
            .field("session_id", &self.setup.session_id)
            .finish_non_exhaustive()
    }
}

impl ComputerUseSession {
    /// The session's computer use; nothing runs until the first action.
    #[must_use]
    pub fn new(setup: SessionSetup) -> Self {
        Self {
            setup,
            yolo: OnceLock::new(),
            state: Mutex::new(State::default()),
        }
    }

    /// Binds the session's yolo state (once the approval gate exists).
    pub fn bind_yolo(&self, probe: YoloProbe) {
        let _ = self.yolo.set(probe);
    }

    /// The setup.
    #[must_use]
    pub fn setup(&self) -> &SessionSetup {
        &self.setup
    }

    /// The label of this session's cursor and driver state.
    #[must_use]
    pub fn label(&self) -> String {
        let id = &self.setup.session_id;
        let short = id.rsplit('_').next().unwrap_or(id);
        format!("prometheus-{}", short.to_lowercase())
    }

    /// The mode a runtime launched now gets.
    #[must_use]
    pub fn desired_mode(&self) -> PermissionMode {
        if self.yolo.get().is_some_and(|yolo| yolo()) {
            PermissionMode::Unrestricted
        } else if self.setup.config.permission_mode == ComputerUsePermission::Bounded {
            PermissionMode::Bounded
        } else {
            PermissionMode::Standard
        }
    }

    /// The mode of the live runtime, if one runs.
    pub async fn running_mode(&self) -> Option<PermissionMode> {
        self.state.lock().await.runtime.as_ref().map(|r| r.mode)
    }

    /// Stops a runtime whose mode no longer matches the session (`/yolo`
    /// turned off): the next action launches the right one.
    pub async fn reconcile(&self) {
        let desired = self.desired_mode();
        let mut state = self.state.lock().await;
        if state.runtime.as_ref().is_some_and(|r| r.mode != desired) {
            stop(&mut state);
        }
    }

    /// Stops the runtime (session end, reset).
    pub async fn stop(&self) {
        stop(&mut *self.state.lock().await);
    }

    /// The session's state, locked for one call, and a runtime of the
    /// right mode (calls run one at a time).
    pub(super) async fn acquire(&self) -> Result<Locked<'_>, String> {
        let mut state = self.state.lock().await;
        let driver = self.runtime(&mut state).await?;
        Ok(Locked { state, driver })
    }

    async fn runtime(&self, state: &mut State) -> Result<Arc<dyn Driver>, String> {
        let desired = self.desired_mode();
        if let Some(runtime) = &state.runtime {
            if runtime.mode == desired {
                return Ok(Arc::clone(&runtime.driver));
            }
            stop(state);
        }
        let spec = RuntimeSpec {
            mode: desired,
            manifest: self.setup.config.capability_manifest.clone(),
            session_label: self.label(),
            overlay: self.setup.overlay,
        };
        let driver = self.setup.launcher.launch(&spec).await?;
        self.prepare(&driver).await;
        state.runtime = Some(Runtime {
            mode: desired,
            driver: Arc::clone(&driver),
        });
        Ok(driver)
    }

    /// Declares the session to the driver and applies the capture size
    /// and overlay; each step is best effort (an older driver lacking one
    /// still drives the desktop).
    async fn prepare(&self, driver: &Arc<dyn Driver>) {
        let label = self.label();
        let mut steps = vec![("start_session", json!({"session": label}))];
        if self.setup.config.max_image_dimension > 0 {
            steps.push((
                "set_config",
                json!({"session": label, "max_image_dimension": self.setup.config.max_image_dimension}),
            ));
        }
        if !self.setup.overlay {
            steps.push((
                "set_agent_cursor_enabled",
                json!({"session": label, "enabled": false, "cursor_id": label}),
            ));
        }
        for (tool, arguments) in steps {
            if let Err(err) = call(driver, tool, arguments).await {
                tracing::debug!(tool, error = %err, "computer-use driver setup step failed");
            }
        }
    }
}

/// Drops the runtime and everything it minted.
fn stop(state: &mut State) {
    state.runtime = None;
    state.target = None;
    state.elements.clear();
}

/// Calls `tool` with an object of arguments.
pub(super) async fn call(
    driver: &Arc<dyn Driver>,
    tool: &str,
    arguments: Value,
) -> Result<DriverReply, String> {
    let arguments = match arguments {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    driver.call(tool, arguments).await
}
