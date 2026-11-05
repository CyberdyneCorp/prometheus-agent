//! Finding and launching a local Chromium (spec: execution-environments,
//! Browser session resolution; distribution-and-update, Lazy runtime
//! installation policy).
//!
//! The executable comes from the managed runtime store when it holds
//! `chromium`, else from a missing runtime installed on first use (when
//! `runtimes.allow_lazy_install` permits), else from a Chromium-family
//! browser on the host. Each launch gets a throwaway profile directory and
//! runs under the OS sandbox ([`super::confine`]) with DevTools on a pipe
//! (`--remote-debugging-pipe`); Windows publishes a loopback DevTools port
//! read back from the profile's `DevToolsActivePort` file.

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::{Child, Command};

use pmt_net::egress::Proxy;

use super::cdp::Cdp;
use super::cleanup;
pub use super::confine::Confinement;
use crate::terminal::env::ChildEnv;

/// Bound on a browser start (`browser.launch_timeout_secs` default).
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);

/// What an install returns.
pub type InstallFuture<'a> = Pin<Box<dyn Future<Output = Result<PathBuf, String>> + Send + 'a>>;

/// The managed runtime store as the browser sees it (the store itself and
/// its pinned index are `prometheus runtimes`).
pub trait Runtimes: Send + Sync {
    /// The installed runtime's executable, if installed.
    fn installed(&self, name: &str) -> Option<PathBuf>;
    /// Installs `name` and returns its executable.
    fn install<'a>(&'a self, name: &'a str) -> InstallFuture<'a>;
    /// Whether `runtimes.exclude` forbids installing `name`.
    fn excluded(&self, _name: &str) -> bool {
        false
    }
}

/// A store with nothing installed and nothing installable (no store yet).
pub struct NoRuntimes;

impl Runtimes for NoRuntimes {
    fn installed(&self, _: &str) -> Option<PathBuf> {
        None
    }

    fn install<'a>(&'a self, name: &'a str) -> InstallFuture<'a> {
        Box::pin(async move { Err(format!("no runtime store can install {name}")) })
    }
}

/// How a missing runtime is obtained.
#[derive(Clone)]
pub struct RuntimePolicy {
    /// The store.
    pub runtimes: Arc<dyn Runtimes>,
    /// `runtimes.allow_lazy_install`.
    pub allow_lazy_install: bool,
    /// Executables on the host to use when the store has none (searched
    /// in order; see [`system_candidates`]).
    pub host_candidates: Vec<PathBuf>,
}

impl RuntimePolicy {
    /// The executable of runtime `name`: installed, else a host binary,
    /// else lazily installed.
    ///
    /// # Errors
    ///
    /// The spec message when the runtime is excluded or lazy installs are
    /// disabled, or the install failure.
    pub async fn executable(&self, name: &str) -> Result<PathBuf, String> {
        if let Some(path) = self.runtimes.installed(name) {
            return Ok(path);
        }
        if let Some(path) = self.host_candidates.iter().find(|p| p.is_file()) {
            return Ok(path.clone());
        }
        self.availability(name).map_err(|_| self.refusal(name))?;
        self.runtimes.install(name).await
    }

    /// The availability probe of a tool that needs runtime `name`: `Err`
    /// carries the `unavailable:<reason>` reason (`excluded`, `lazy
    /// installs disabled`) when using the tool would need an install the
    /// policy forbids.
    ///
    /// # Errors
    ///
    /// The reason.
    pub fn availability(&self, name: &str) -> Result<(), &'static str> {
        let present = self.runtimes.installed(name).is_some()
            || self.host_candidates.iter().any(|p| p.is_file());
        if present {
            Ok(())
        } else if self.runtimes.excluded(name) {
            Err("excluded")
        } else if !self.allow_lazy_install {
            Err("lazy installs disabled")
        } else {
            Ok(())
        }
    }

    fn refusal(&self, name: &str) -> String {
        if self.runtimes.excluded(name) {
            format!(
                "{name} is excluded by runtimes.exclude: run 'prometheus runtimes exclude remove {name}' to allow it"
            )
        } else {
            format!(
                "{name} is not installed and lazy installs are disabled: run 'prometheus runtimes install {name}'"
            )
        }
    }
}

/// Chromium-family browsers commonly installed on this host type, plus
/// the same names on `path_var`.
#[must_use]
pub fn system_candidates(path_var: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if cfg!(target_os = "macos") {
        for app in [
            "Google Chrome.app/Contents/MacOS/Google Chrome",
            "Chromium.app/Contents/MacOS/Chromium",
            "Brave Browser.app/Contents/MacOS/Brave Browser",
            "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        ] {
            found.push(Path::new("/Applications").join(app));
        }
    }
    if cfg!(windows) {
        for root in ["C:\\Program Files", "C:\\Program Files (x86)"] {
            for exe in [
                "Google\\Chrome\\Application\\chrome.exe",
                "Microsoft\\Edge\\Application\\msedge.exe",
                "BraveSoftware\\Brave-Browser\\Application\\brave.exe",
            ] {
                found.push(Path::new(root).join(exe));
            }
        }
    }
    let names = [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
        "brave-browser",
        "microsoft-edge",
    ];
    if let Some(path_var) = path_var {
        for name in names {
            found.extend(crate::terminal::shell::find_all(name, path_var, None));
        }
    }
    found
}

/// A launched local browser.
pub struct Launched {
    /// The process (killed on drop).
    pub child: Child,
    /// Its pid.
    pub pid: u32,
    /// The DevTools connection.
    pub cdp: Arc<Cdp>,
    /// The throwaway profile directory.
    pub profile_dir: PathBuf,
    /// The browser's short temporary directory.
    pub temp_dir: PathBuf,
}

impl Launched {
    /// Kills the browser and its helpers, waits for it to exit and removes
    /// its profile and temporary directories.
    pub async fn terminate(mut self) {
        cleanup::kill_group(self.pid);
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
        cleanup::remove_dir_async(&self.profile_dir).await;
        cleanup::remove_dir_async(&self.temp_dir).await;
    }
}

/// Launches `executable` headless on a fresh throwaway profile directory
/// under `profiles_root` with `env` as its whole environment, under the
/// OS sandbox of `confinement` (spec: execution-environments, Local
/// browser isolation). Every connection the browser makes goes through
/// `proxy`, the session's egress proxy, loopback included (spec:
/// security-boundaries, the egress policy covers every outbound
/// connection made by browsers): Chromium's implicit bypass of
/// `localhost` would otherwise reach local services unchecked; where the
/// sandbox confines the network the OS enforces it too.
///
/// # Errors
///
/// The sandbox cannot be built, the spawn failed, or the browser exited
/// or did not answer over DevTools within `timeout`.
pub async fn launch(
    executable: &Path,
    profiles_root: &Path,
    env: &ChildEnv,
    timeout: Duration,
    proxy: &Proxy,
    confinement: &Confinement,
) -> Result<Launched, String> {
    let profile_dir = profiles_root.join(ulid::Ulid::generate().to_string().to_lowercase());
    pmt_core::private_fs::create_dir(&profile_dir)
        .map_err(|err| format!("cannot create the browser profile: {err}"))?;
    // Chromium puts its singleton socket under its temp directory, and a
    // Unix socket path must stay under about 100 bytes: a session scratch
    // directory is too deep, so the browser gets a short directory in the
    // host's temp.
    let temp_dir = std::env::temp_dir().join(format!(
        "pmt-browser-{}",
        &ulid::Ulid::generate().to_string().to_lowercase()[16..]
    ));
    if let Err(err) = pmt_core::private_fs::create_dir(&temp_dir) {
        cleanup::remove_dir_async(&profile_dir).await;
        return Err(format!("cannot create the browser's temp directory: {err}"));
    }
    let dirs = Dirs {
        profile: profile_dir.clone(),
        temp: temp_dir.clone(),
    };
    match start(executable, &dirs, env, timeout, proxy, confinement).await {
        Ok((child, pid, cdp)) => Ok(Launched {
            child,
            pid,
            cdp,
            profile_dir,
            temp_dir,
        }),
        Err(why) => {
            cleanup::remove_dir_async(&profile_dir).await;
            cleanup::remove_dir_async(&temp_dir).await;
            Err(why)
        }
    }
}

/// The browser's own directories.
struct Dirs {
    profile: PathBuf,
    temp: PathBuf,
}

/// The browser command line shared by every platform.
fn base_command(command: &mut std::process::Command, dirs: &Dirs, env: &ChildEnv, proxy: &Proxy) {
    let mut env = env.clone();
    env.set_temp_dir(&dirs.temp);
    env.apply(command);
    let mut flag = OsString::from("--user-data-dir=");
    flag.push(&dirs.profile);
    command
        .arg("--headless=new")
        .arg(flag)
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-sync",
            "--password-store=basic",
            "--use-mock-keychain",
        ])
        .arg(format!("--proxy-server={}", proxy.url()))
        .arg("--proxy-bypass-list=<-loopback>")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cleanup::new_group(command);
}

/// Spawns the browser and waits until it answers over DevTools, killing
/// it when it does not.
async fn start(
    executable: &Path,
    dirs: &Dirs,
    env: &ChildEnv,
    timeout: Duration,
    proxy: &Proxy,
    confinement: &Confinement,
) -> Result<(Child, u32, Arc<Cdp>), String> {
    let Spawned { mut child, connect } = spawn(executable, dirs, env, proxy, confinement)?;
    let pid = child.id().unwrap_or_default();
    let log = StderrTail::drain(child.stderr.take());
    match connect.ready(&mut child, &dirs.profile, timeout).await {
        Ok(cdp) => Ok((child, pid, cdp)),
        Err(why) => {
            cleanup::kill_group(pid);
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(log.explain(why).await)
        }
    }
}

/// A started browser and how to reach its DevTools.
struct Spawned {
    child: Child,
    connect: Connect,
}

/// How DevTools is reached.
enum Connect {
    /// `--remote-debugging-pipe` (Unix): no listening port at all, so a
    /// browser in its own network namespace stays reachable.
    #[cfg(unix)]
    Pipe(Arc<Cdp>),
    /// `--remote-debugging-port=0` (Windows): the endpoint is read back
    /// from the profile's `DevToolsActivePort` file.
    #[cfg(not(unix))]
    Port,
}

impl Connect {
    async fn ready(
        self,
        child: &mut Child,
        profile_dir: &Path,
        timeout: Duration,
    ) -> Result<Arc<Cdp>, String> {
        match self {
            #[cfg(unix)]
            Self::Pipe(cdp) => {
                let _ = profile_dir;
                let probe = Arc::clone(&cdp);
                let answer = tokio::select! {
                    answer = probe.call_within("Browser.getVersion", serde_json::json!({}), None, timeout) => answer,
                    status = child.wait() => return Err(exited(status)),
                };
                match answer {
                    Ok(_) => Ok(cdp),
                    // A closed pipe is usually a browser that just exited.
                    Err(why) => {
                        match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
                            Ok(status) => Err(exited(status)),
                            Err(_) if why.contains("timed out") => Err(format!(
                                "the browser did not start within {} s",
                                timeout.as_secs()
                            )),
                            Err(_) => Err(format!("the browser did not start: {why}")),
                        }
                    }
                }
            }
            #[cfg(not(unix))]
            Self::Port => {
                let endpoint = wait_for_endpoint(child, profile_dir, timeout).await?;
                Cdp::connect(&endpoint).await
            }
        }
    }
}

/// The start failure of a browser that exited.
#[cfg(unix)]
fn exited(status: std::io::Result<std::process::ExitStatus>) -> String {
    match status {
        Ok(status) => format!("the browser exited during start ({status})"),
        Err(err) => format!("the browser exited during start ({err})"),
    }
}

#[cfg(unix)]
fn spawn(
    executable: &Path,
    dirs: &Dirs,
    env: &ChildEnv,
    proxy: &Proxy,
    confinement: &Confinement,
) -> Result<Spawned, String> {
    let sandbox =
        super::confine::sandbox(executable, &dirs.profile, &dirs.temp, proxy, confinement)?;
    let mut std_command = sandbox
        .command(executable)
        .map_err(|err| err.message().to_owned())?;
    base_command(&mut std_command, dirs, env, proxy);
    // Chromium's own sandbox cannot start inside the OS sandbox (nested
    // Seatbelt profiles and user namespaces are refused); the OS sandbox
    // confines the whole process tree instead.
    if sandbox.confines() {
        std_command.arg("--no-sandbox");
    }
    // Everything the browser would write under the user's home (config,
    // caches, crash database) goes to its private temp directory instead:
    // the home is read-only in the sandbox, and Chromium aborts when its
    // crash handler cannot open a database. The crash handler is not
    // started at all (it would need ptrace and a writable database).
    let mut crash_dir = OsString::from("--crash-dumps-dir=");
    crash_dir.push(&dirs.temp);
    std_command
        .env("MAC_CHROMIUM_TMPDIR", &dirs.temp)
        .env("XDG_CONFIG_HOME", &dirs.temp)
        .env("XDG_CACHE_HOME", &dirs.temp)
        .args([
            "--remote-debugging-pipe",
            "--disable-gpu",
            "--disable-dev-shm-usage",
            "--disable-crash-reporter",
            "--disable-crashpad-for-testing",
            "--disable-breakpad",
        ])
        .arg(crash_dir)
        .arg("about:blank");
    let pipe = |err: std::io::Error| format!("cannot create the DevTools pipe: {err}");
    let (to_browser_read, to_browser_write) = std::io::pipe().map_err(pipe)?;
    let (from_browser_read, from_browser_write) = std::io::pipe().map_err(pipe)?;
    pmt_sandbox::fds::pass(
        &mut std_command,
        vec![(to_browser_read.into(), 3), (from_browser_write.into(), 4)],
    )
    .map_err(pipe)?;
    let mut command = Command::from(std_command);
    command.kill_on_drop(true);
    let child = command
        .spawn()
        .map_err(|err| format!("cannot start {}: {err}", executable.display()))?;
    // The command holds the browser's ends of the pipes: drop it so the
    // pipe reports the browser's exit.
    drop(command);
    let label = format!("pipe:{}", child.id().unwrap_or_default());
    let cdp = Cdp::over_pipe(&label, to_browser_write.into(), from_browser_read.into())?;
    Ok(Spawned {
        child,
        connect: Connect::Pipe(cdp),
    })
}

/// Windows has no descriptor passing to a child at fixed numbers: the
/// browser publishes a loopback DevTools port and runs unconfined there.
#[cfg(not(unix))]
fn spawn(
    executable: &Path,
    dirs: &Dirs,
    env: &ChildEnv,
    proxy: &Proxy,
    _confinement: &Confinement,
) -> Result<Spawned, String> {
    let mut std_command = std::process::Command::new(executable);
    base_command(&mut std_command, dirs, env, proxy);
    std_command
        .arg("--remote-debugging-port=0")
        .arg("about:blank");
    let mut command = Command::from(std_command);
    command.kill_on_drop(true);
    let child = command
        .spawn()
        .map_err(|err| format!("cannot start {}: {err}", executable.display()))?;
    Ok(Spawned {
        child,
        connect: Connect::Port,
    })
}

/// The last lines the browser wrote to stderr. The pipe is drained for the
/// browser's whole life (a full pipe would block it); the tail explains a
/// failed start (for example Chromium's `No usable sandbox!`).
struct StderrTail {
    lines: Arc<std::sync::Mutex<Kept>>,
    reader: Option<tokio::task::JoinHandle<()>>,
}

/// The first fatal line and the last lines of browser stderr.
#[derive(Default)]
struct Kept {
    fatal: Option<String>,
    tail: std::collections::VecDeque<String>,
}

impl Kept {
    fn push(&mut self, line: String) {
        let fatal = line.contains("FATAL") || line.contains("Check failed");
        if fatal && self.fatal.is_none() {
            self.fatal = Some(line.clone());
        }
        if self.tail.len() == TAIL_LINES {
            self.tail.pop_front();
        }
        self.tail.push_back(line);
    }
}

/// Lines of browser stderr kept for an error message.
const TAIL_LINES: usize = 8;

impl StderrTail {
    fn drain(stderr: Option<tokio::process::ChildStderr>) -> Self {
        use tokio::io::AsyncBufReadExt as _;
        let lines: Arc<std::sync::Mutex<Kept>> = Arc::default();
        let reader = stderr.map(|stderr| {
            let kept = Arc::clone(&lines);
            tokio::spawn(async move {
                let mut reader = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    kept.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(line);
                }
            })
        });
        Self { lines, reader }
    }

    /// `why` followed by the browser's last stderr lines, once it exited.
    async fn explain(mut self, why: String) -> String {
        if let Some(reader) = self.reader.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), reader).await;
        }
        let kept = self
            .lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut shown: Vec<&str> = kept.fatal.iter().map(String::as_str).collect();
        shown.extend(
            kept.tail
                .iter()
                .map(String::as_str)
                .filter(|line| Some(*line) != kept.fatal.as_deref()),
        );
        if shown.is_empty() {
            return why;
        }
        format!("{why}; browser output: {}", shown.join(" | "))
    }
}

/// Polls `DevToolsActivePort` (port, then path) until the browser writes it.
#[cfg(not(unix))]
async fn wait_for_endpoint(
    child: &mut Child,
    profile_dir: &Path,
    timeout: Duration,
) -> Result<String, String> {
    let file = profile_dir.join("DevToolsActivePort");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(text) = std::fs::read_to_string(&file) {
            let mut lines = text.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                return Ok(format!("ws://127.0.0.1:{}{}", port.trim(), path.trim()));
            }
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("the browser exited during start ({status})"));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "the browser did not start within {} s",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
