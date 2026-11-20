//! Launching `cua-driver` runtimes (spec: computer-use, Permission modes
//! are fixed per runtime; Driver runtime management and identity; Which
//! machine is driven; Agent cursor overlay).
//!
//! The driver runs where the session's screen lives, as
//! `bot_desktop.placement` resolves it: on this host for the `local`
//! backend, inside the profile's desktop container (the
//! `prometheus-sandbox-desktop` image carries the driver) or on the SSH
//! host for those backends, refused for backends that cannot host a
//! display. A `standard` runtime is the driver's `mcp` server; a private
//! (`bounded` or `unrestricted`) runtime is a `serve --embedded` daemon of
//! its own, launched with its mode and manifest fixed, plus an `mcp` proxy
//! to its socket; on macOS the daemon starts through the signature-checked
//! `CuaDriver.app` so the privacy grants stay attached to its identity.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use pmt_config::ProfileScope;
use pmt_config::daemon::DaemonConfig;
use pmt_mcp::computer_use::McpDriver;
use pmt_tools::computer_use::{Driver, Launcher, PermissionMode, RuntimeSpec};
use pmt_tools::dispatch::SeamFuture;
use pmt_update::runtimes::Store;

use super::binary::{self, INSTALL_HINT, RUNTIME, Source};
use crate::screen::placement::{self, Hosting, Where};

/// How long a runtime may take to answer `initialize`.
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a private daemon may take to listen.
const DAEMON_START: Duration = Duration::from_secs(15);
/// The telemetry switch the driver reads.
pub const TELEMETRY_VAR: &str = "CUA_DRIVER_RS_TELEMETRY_ENABLED";
/// The native Wayland switch the driver reads.
const WAYLAND_VAR: &str = "CUA_DRIVER_RS_ENABLE_WAYLAND";
/// Launch variables a driver may see (never secrets or Prometheus' own).
const PASSED: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "TZ",
    "XDG_RUNTIME_DIR",
    "XDG_SESSION_TYPE",
    "DBUS_SESSION_BUS_ADDRESS",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "SystemRoot",
    "windir",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "TEMP",
    "TMP",
    "SESSIONNAME",
];
/// Runs a private runtime inside a container or on an SSH host: the
/// daemon, then the MCP proxy to its socket; the daemon stops with it.
const PRIVATE_SCRIPT: &str = r#"sock="${TMPDIR:-/tmp}/pmt-cua-$$.sock"
cua-driver serve --embedded --socket "$sock" "$@" >/dev/null 2>&1 &
daemon=$!
trap 'cua-driver stop --socket "$sock" >/dev/null 2>&1; kill $daemon 2>/dev/null; rm -f "$sock"' EXIT
i=0
until cua-driver status --socket "$sock" >/dev/null 2>&1; do
  i=$((i+1)); [ "$i" -gt 150 ] && { echo "the private cua-driver did not start" >&2; exit 69; }
  sleep 0.1
done
cua-driver mcp --embedded --socket "$sock""#;

/// The driver environment: the passed launch variables, telemetry off
/// unless opted in, native Wayland when asked for.
#[must_use]
pub fn driver_env(
    scope: &ProfileScope,
    launch: &dyn Fn(&str) -> Option<OsString>,
) -> Vec<(String, OsString)> {
    let config = &scope.config().computer_use;
    let mut env: Vec<(String, OsString)> = PASSED
        .iter()
        .filter_map(|name| launch(name).map(|value| ((*name).to_owned(), value)))
        .collect();
    if !config.driver_telemetry {
        env.push((TELEMETRY_VAR.to_owned(), "0".into()));
    }
    if config.native_wayland && launch("WAYLAND_DISPLAY").is_some() {
        env.push((WAYLAND_VAR.to_owned(), "1".into()));
    }
    env
}

fn set(env: &mut Vec<(String, OsString)>, name: &str, value: impl Into<OsString>) {
    env.retain(|(k, _)| k != name);
    env.push((name.to_owned(), value.into()));
}

/// The `cua` backend's launcher for one session.
pub struct CuaLauncher {
    scope: ProfileScope,
    env: Vec<(String, OsString)>,
    /// A managed driver failing the contract is repaired once per session.
    repaired: AtomicBool,
}

impl CuaLauncher {
    /// The launcher of a session of `scope` whose driver sees `env`.
    #[must_use]
    pub fn new(scope: ProfileScope, env: Vec<(String, OsString)>) -> Self {
        Self {
            scope,
            env,
            repaired: AtomicBool::new(false),
        }
    }

    async fn launch_now(&self, spec: &RuntimeSpec) -> Result<Arc<dyn Driver>, String> {
        let config = self.scope.config();
        let place =
            placement::resolve(&config.bot_desktop, &config.terminal.backend).map_err(|e| {
                let hint = e.hint().map(|h| format!(" ({h})")).unwrap_or_default();
                format!(
                    "computer use cannot drive this session's screen: {}{hint}",
                    e.message()
                )
            })?;
        if spec.mode == PermissionMode::Bounded {
            check_manifest(spec.manifest.as_deref())?;
        }
        let driver: McpDriver = match place {
            Where::Host => self.on_host(spec).await?,
            Where::Backend(hosting) => self.in_backend(hosting, spec).await?,
        };
        Ok(Arc::new(driver))
    }

    /// The driver of this host, installed and checked.
    async fn host_driver(&self) -> Result<(PathBuf, binary::Manifest), String> {
        let home = self.scope.home().to_path_buf();
        let daemon = DaemonConfig::load(&home).map_err(|e| e.message().to_owned())?;
        let located = binary::installed(&home, &self.scope.config().computer_use, &daemon)?;
        let driver = if let Some(driver) = located {
            driver
        } else {
            self.install(&daemon).await?;
            binary::installed(&home, &self.scope.config().computer_use, &daemon)?
                .ok_or_else(|| format!("{RUNTIME} is not installed: {INSTALL_HINT}"))?
        };
        match binary::checked(&driver.path, &self.env) {
            Ok(manifest) => Ok((driver.path, manifest)),
            Err(problem) if driver.source == Source::Configured => Err(format!(
                "{problem}; computer_use.driver_path {} is left as it is: update it or unset the key",
                driver.path.display()
            )),
            Err(problem) if self.repaired.swap(true, Ordering::SeqCst) => Err(problem),
            Err(problem) => {
                tracing::warn!(%problem, "repairing the managed cua-driver");
                Store::open(&home)
                    .remove(RUNTIME)
                    .map_err(|e| e.message().to_owned())?;
                self.install(&daemon).await?;
                let path = Store::open(&home)
                    .installed(RUNTIME)
                    .map(|i| i.exe())
                    .ok_or_else(|| format!("{RUNTIME} repair did not install it"))?;
                let manifest = binary::checked(&path, &self.env)
                    .map_err(|again| format!("{again} (after one repair this session)"))?;
                Ok((path, manifest))
            }
        }
    }

    async fn install(&self, daemon: &DaemonConfig) -> Result<(), String> {
        let progress: pmt_update::runtimes::install::Progress =
            Arc::new(|line| tracing::info!(target: "computer_use", "{line}"));
        let installer = crate::runtimes_cmd::browser::installer(
            Store::open(self.scope.home()),
            daemon,
            &crate::runtimes_cmd::browser::outbound(&self.scope.config().network),
            progress,
        );
        installer
            .install(RUNTIME)
            .await
            .map(|_| ())
            .map_err(|e| format!("{}: {INSTALL_HINT}", e.message()))
    }

    /// This host's environment plus the bot screen's display, started
    /// when nothing else provides one on Linux.
    fn host_env(&self) -> Vec<(String, OsString)> {
        let mut env = self.env.clone();
        let record = match crate::screen::running(&self.scope) {
            Some((record, _)) => Some(record),
            None if cfg!(target_os = "linux") && !env.iter().any(|(k, _)| k == "DISPLAY") => {
                crate::screen::for_headed_browser(&self.scope)
            }
            None => None,
        };
        if let Some(record) = record.filter(|r| r.placement == "host") {
            set(&mut env, "DISPLAY", record.display);
            if let Some(auth) = record.xauthority {
                set(&mut env, "XAUTHORITY", auth);
            }
        }
        env
    }

    async fn on_host(&self, spec: &RuntimeSpec) -> Result<McpDriver, String> {
        let (driver, manifest) = self.host_driver().await?;
        let env = self.host_env();
        let program = manifest
            .mcp_command
            .clone()
            .unwrap_or_else(|| driver.clone());
        let no_overlay = (!spec.overlay && manifest.no_overlay).then_some("--no-overlay");
        if !spec.mode.is_private() {
            let mut command = tokio::process::Command::new(&program);
            command
                .args(&manifest.mcp_args)
                .args(no_overlay)
                .env_clear()
                .envs(env.iter().map(|(k, v)| (k, v)));
            return McpDriver::open(command, OPEN_TIMEOUT, Box::new(())).await;
        }
        let daemon = PrivateDaemon::start(&driver, spec, no_overlay, &env, &self.scope)?;
        let mut command = tokio::process::Command::new(&program);
        command
            .args(&manifest.mcp_args)
            .args(["--embedded", "--socket"])
            .arg(&daemon.socket)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)));
        McpDriver::open(command, OPEN_TIMEOUT, Box::new(daemon)).await
    }

    /// The driver inside the container or on the SSH host, where the
    /// session's screen runs.
    async fn in_backend(&self, hosting: Hosting, spec: &RuntimeSpec) -> Result<McpDriver, String> {
        let record = crate::screen::for_headed_browser(&self.scope).ok_or_else(|| {
            "the bot screen inside the terminal backend is not running: start it with `prometheus computer-use screen start` (or set bot_desktop.auto_start = true)".to_owned()
        })?;
        let display = record
            .backend_display
            .clone()
            .unwrap_or_else(|| ":1".to_owned());
        let telemetry_off = !self.scope.config().computer_use.driver_telemetry;
        let argv = backend_argv(spec);
        let command = match hosting {
            Hosting::Container => {
                let config = self.scope.config();
                let runtime =
                    pmt_sandbox::container::Runtime::detect(&config.terminal.container.runtime)
                        .map_err(|e| e.message().to_owned())?;
                let mut command = runtime.command();
                command.args(["exec", "-i"]);
                command.arg(format!("--env=DISPLAY={display}"));
                command.arg("--env=XAUTHORITY=/run/pmt-xauth/Xauthority");
                if telemetry_off {
                    command.arg(format!("--env={TELEMETRY_VAR}=0"));
                }
                command
                    .arg(crate::screen::container_name(&self.scope))
                    .args(&argv);
                command
            }
            Hosting::Ssh => {
                let config = self.scope.config();
                let control = self.scope.path("run/ssh");
                let target =
                    pmt_sandbox::ssh::SshTarget::from_config(&config.terminal.ssh, Some(&control))
                        .map_err(|e| e.message().to_owned())?;
                let telemetry = if telemetry_off {
                    format!(" {TELEMETRY_VAR}=0")
                } else {
                    String::new()
                };
                let mut remote = vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    format!(
                        "export DISPLAY=\"$1\" XAUTHORITY=\"${{TMPDIR:-/tmp}}/pmt-bot-$2/Xauthority\"{telemetry}; shift 2; exec \"$@\""
                    ),
                    "sh".to_owned(),
                    display,
                    crate::screen::short_id(&self.scope),
                ];
                remote.extend(argv);
                target.command(&remote)
            }
        };
        McpDriver::open(
            tokio::process::Command::from(command),
            OPEN_TIMEOUT,
            Box::new(()),
        )
        .await
    }
}

/// The driver command run inside a backend: `cua-driver mcp`, or the
/// private daemon script for a private mode.
fn backend_argv(spec: &RuntimeSpec) -> Vec<String> {
    let mut argv: Vec<String> = if spec.mode.is_private() {
        let mut argv: Vec<String> = ["sh", "-c", PRIVATE_SCRIPT, "sh"].map(str::to_owned).into();
        argv.extend(serve_args(spec, None));
        argv
    } else {
        vec!["cua-driver".to_owned(), "mcp".to_owned()]
    };
    if !spec.overlay {
        argv.push("--no-overlay".to_owned());
    }
    argv
}

impl Launcher for CuaLauncher {
    fn launch<'a>(
        &'a self,
        spec: &'a RuntimeSpec,
    ) -> SeamFuture<'a, Result<Arc<dyn Driver>, String>> {
        Box::pin(self.launch_now(spec))
    }
}

/// A bounded runtime needs a readable manifest; it never runs unbounded.
///
/// # Errors
///
/// The missing or unreadable path.
pub fn check_manifest(manifest: Option<&Path>) -> Result<(), String> {
    let Some(path) = manifest else {
        return Err(
            "computer_use.permission_mode = \"bounded\" needs computer_use.capability_manifest"
                .to_owned(),
        );
    };
    std::fs::read(path).map(|_| ()).map_err(|e| {
        format!(
            "the capability manifest {} cannot be read: {e}",
            path.display()
        )
    })
}

/// `serve` arguments after `--socket <socket>` (when given).
fn serve_args(spec: &RuntimeSpec, manifest_applies: Option<bool>) -> Vec<String> {
    let mut args = vec![
        "--no-permissions-gate".to_owned(),
        "--permission-mode".to_owned(),
        spec.mode.as_str().to_owned(),
    ];
    if spec.mode == PermissionMode::Unrestricted {
        args.push("--dangerously-bypass-approvals".to_owned());
    }
    if let Some(manifest) = &spec.manifest
        && manifest_applies.unwrap_or(spec.mode == PermissionMode::Bounded)
    {
        args.extend([
            "--capability-manifest".to_owned(),
            manifest.display().to_string(),
            "--approve-capability-manifest".to_owned(),
        ]);
    }
    args
}

/// A private runtime's daemon on this host; dropping it stops the daemon.
struct PrivateDaemon {
    driver: PathBuf,
    socket: PathBuf,
    child: Option<std::process::Child>,
    env: Vec<(String, OsString)>,
}

impl PrivateDaemon {
    fn start(
        driver: &Path,
        spec: &RuntimeSpec,
        no_overlay: Option<&str>,
        env: &[(String, OsString)],
        scope: &ProfileScope,
    ) -> Result<Self, String> {
        let socket = socket_path();
        let mut args: Vec<String> = vec![
            "serve".into(),
            "--embedded".into(),
            "--socket".into(),
            socket.display().to_string(),
        ];
        args.extend(serve_args(spec, None));
        args.extend(no_overlay.map(str::to_owned));
        let mut command = if cfg!(target_os = "macos") {
            let app = super::identity::bundle(driver).ok_or_else(|| {
                format!("private computer-use runtimes on macOS start only through CuaDriver.app, and {} is not inside one: {INSTALL_HINT}", driver.display())
            })?;
            super::identity::verify(&app, scope.config().computer_use.allow_unsigned_driver)?;
            let mut open = std::process::Command::new("/usr/bin/open");
            open.args(["-n", "-g", "-a"])
                .arg(&app)
                .arg("--args")
                .args(&args);
            open
        } else {
            let mut serve = std::process::Command::new(driver);
            serve.args(&args);
            serve
        };
        command
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let child = command
            .spawn()
            .map_err(|e| format!("could not start the private cua-driver: {e}"))?;
        let mut daemon = Self {
            driver: driver.to_path_buf(),
            socket,
            child: Some(child),
            env: env.to_vec(),
        };
        daemon.wait_ready()?;
        Ok(daemon)
    }

    /// Polls `status --socket` until the daemon answers (the driver prints
    /// no ready line); a daemon that exits first fails at once.
    fn wait_ready(&mut self) -> Result<(), String> {
        let socket = self.socket.display().to_string();
        let deadline = Instant::now() + DAEMON_START;
        while Instant::now() < deadline {
            if binary::verb(&self.driver, &["status", "--socket", &socket], &self.env).is_ok() {
                return Ok(());
            }
            let exited = self
                .child
                .as_mut()
                .and_then(|c| c.try_wait().ok().flatten());
            // `open` exits 0 once it handed the bundle to LaunchServices.
            if let Some(status) = exited.filter(|s| !(cfg!(target_os = "macos") && s.success())) {
                return Err(format!(
                    "the private cua-driver exited during startup ({status})"
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(format!(
            "the private cua-driver did not listen on {socket} within {} s",
            DAEMON_START.as_secs()
        ))
    }
}

impl Drop for PrivateDaemon {
    fn drop(&mut self) {
        let socket = self.socket.display().to_string();
        let _ = binary::verb(&self.driver, &["stop", "--socket", &socket], &self.env);
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if !cfg!(windows) {
            let _ = std::fs::remove_file(&self.socket);
        }
    }
}

/// A short, unique socket path (Unix socket paths stay under ~100 bytes).
fn socket_path() -> PathBuf {
    let id = ulid::Ulid::generate().to_string().to_lowercase();
    let short = &id[id.len() - 12..];
    if cfg!(windows) {
        PathBuf::from(format!(r"\\.\pipe\pmt-cua-{short}"))
    } else {
        std::env::temp_dir().join(format!("pmt-cua-{short}.sock"))
    }
}

#[cfg(test)]
mod tests;
