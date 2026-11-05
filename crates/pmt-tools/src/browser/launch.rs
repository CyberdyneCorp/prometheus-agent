//! Finding and launching a local Chromium (spec: execution-environments,
//! Browser session resolution; distribution-and-update, Lazy runtime
//! installation policy).
//!
//! The executable comes from the managed runtime store when it holds
//! `chromium`, else from a missing runtime installed on first use (when
//! `runtimes.allow_lazy_install` permits), else from a Chromium-family
//! browser on the host. Each launch gets a throwaway profile directory and
//! runs under the OS sandbox ([`super::confine`]) with DevTools on a pipe
//! (`--remote-debugging-pipe`): fds 3 and 4 on Unix, inherited handles
//! named by `--remote-debugging-io-pipes` on Windows.

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
pub use super::options::LaunchOptions;
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
///
/// None on Windows: a local browser runs in an `AppContainer` there, and
/// every full Chromium build (Chrome, Edge, Brave, Chromium) starts its
/// crashpad handler before reading any switch, on a named pipe outside the
/// container's pipe namespace, and aborts (`CreateNamedPipe: Access is
/// denied`). The managed runtime, `chrome-headless-shell`, starts crash
/// reporting only on `--enable-crash-reporter`, so it is the browser there.
#[must_use]
pub fn system_candidates(path_var: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if cfg!(windows) {
        return found;
    }
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
    /// The profile directory.
    pub profile_dir: PathBuf,
    /// The profile directory is `browser.user_data_dir` (or a real-profile
    /// snapshot) and outlives the browser.
    pub keep_profile: bool,
    /// The browser's short temporary directory.
    pub temp_dir: PathBuf,
    /// Its OS sandbox, kept while the browser runs (on Windows dropping it
    /// removes the `AppContainer` and its file entries).
    pub sandbox: pmt_sandbox::Sandbox,
}

impl Launched {
    /// Kills the browser and its helpers, waits for it to exit and removes
    /// its profile and temporary directories.
    pub async fn terminate(mut self) {
        cleanup::kill_group(self.pid);
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
        if !self.keep_profile {
            cleanup::remove_dir_async(&self.profile_dir).await;
        }
        cleanup::remove_dir_async(&self.temp_dir).await;
    }
}

/// Launches `executable` on a fresh throwaway profile directory under
/// `profiles_root` (or `options.user_data_dir`, kept afterwards), headless
/// unless `options.headed`, with `env` as its whole environment, under the
/// OS sandbox of `confinement` (spec: execution-environments, Local
/// browser isolation; Local browser options and headed mode). Every
/// connection the browser makes goes through `proxy`, the session's
/// egress proxy, loopback included (spec: security-boundaries, the egress
/// policy covers every outbound connection made by browsers): Chromium's
/// implicit bypass of `localhost` would otherwise reach local services
/// unchecked; where the sandbox confines the network the OS enforces it
/// too.
///
/// # Errors
///
/// The sandbox cannot be built, the spawn failed, or the browser exited
/// or did not answer over DevTools within `options.timeout`.
pub async fn launch(
    executable: &Path,
    profiles_root: &Path,
    env: &ChildEnv,
    proxy: &Proxy,
    confinement: &Confinement,
    options: &LaunchOptions,
) -> Result<Launched, String> {
    let keep_profile = options.user_data_dir.is_some();
    let profile_dir = options
        .user_data_dir
        .clone()
        .unwrap_or_else(|| profiles_root.join(ulid::Ulid::generate().to_string().to_lowercase()));
    pmt_core::private_fs::create_dir(&profile_dir)
        .map_err(|err| format!("cannot create the browser profile: {err}"))?;
    let throwaway = |dir: PathBuf| async move {
        if !keep_profile {
            cleanup::remove_dir_async(&dir).await;
        }
    };
    let temp_dir = temp_root(profiles_root).join(format!(
        "pmt-browser-{}",
        &ulid::Ulid::generate().to_string().to_lowercase()[16..]
    ));
    if let Err(err) = pmt_core::private_fs::create_dir(&temp_dir) {
        throwaway(profile_dir).await;
        return Err(format!("cannot create the browser's temp directory: {err}"));
    }
    let dirs = Dirs {
        profile: profile_dir.clone(),
        temp: temp_dir.clone(),
    };
    let run = Run {
        executable,
        dirs: &dirs,
        env,
        proxy,
        confinement,
        options,
    };
    match start(&run).await {
        Ok((Spawned { child, sandbox, .. }, pid, cdp)) => Ok(Launched {
            child,
            pid,
            cdp,
            profile_dir,
            keep_profile,
            temp_dir,
            sandbox,
        }),
        Err(why) => {
            throwaway(profile_dir).await;
            cleanup::remove_dir_async(&temp_dir).await;
            Err(why)
        }
    }
}

/// Everything one browser start uses.
pub(super) struct Run<'a> {
    pub(super) executable: &'a Path,
    dirs: &'a Dirs,
    env: &'a ChildEnv,
    pub(super) proxy: &'a Proxy,
    pub(super) confinement: &'a Confinement,
    pub(super) options: &'a LaunchOptions,
}

/// Where a browser's temp directory goes. Unix: the host's temp, since
/// Chromium puts its singleton socket there and a socket path must stay
/// under about 100 bytes (a profile cache is too deep). Windows: beside the
/// profiles, never below the user's `AppData`, whose directories (the
/// `AppContainer` profiles' parent among them) the sandbox would otherwise
/// rewrite listing entries on.
fn temp_root(profiles_root: &Path) -> PathBuf {
    if cfg!(windows) {
        profiles_root.parent().unwrap_or(profiles_root).join("tmp")
    } else {
        std::env::temp_dir()
    }
}

/// The browser's own directories.
struct Dirs {
    profile: PathBuf,
    temp: PathBuf,
}

/// The browser command line shared by every platform.
fn base_command(command: &mut std::process::Command, run: &Run<'_>) {
    let dirs = run.dirs;
    let mut env = run.env.clone();
    env.set_temp_dir(&dirs.temp);
    for (name, value) in &run.options.display_env {
        env.set(name, value);
    }
    env.apply(command);
    let mut flag = OsString::from("--user-data-dir=");
    flag.push(&dirs.profile);
    if !run.options.headed {
        command.arg("--headless=new");
    }
    command
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
        .arg(format!("--proxy-server={}", run.proxy.url()))
        .arg("--proxy-bypass-list=<-loopback>")
        // A directory the sandbox lets the browser use: an AppContainer
        // process cannot even be created in a directory it cannot reach.
        .current_dir(&dirs.temp)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command.args(&run.options.extra_args);
    cleanup::new_group(command);
}

/// Spawns the browser and waits until it answers over DevTools, killing
/// it when it does not.
async fn start(run: &Run<'_>) -> Result<(Spawned, u32, Arc<Cdp>), String> {
    let mut spawned = spawn(run)?;
    let pid = spawned.child.id().unwrap_or_default();
    let log = StderrTail::drain(spawned.child.stderr.take());
    match spawned
        .connect
        .ready(&mut spawned.child, &run.dirs.profile, run.options.timeout)
        .await
    {
        Ok(cdp) => Ok((spawned, pid, cdp)),
        Err(why) => {
            cleanup::kill_group(pid);
            let _ = spawned.child.start_kill();
            let _ = spawned.child.wait().await;
            Err(log.explain(why).await)
        }
    }
}

/// A started browser and how to reach its DevTools.
struct Spawned {
    child: Child,
    connect: Connect,
    sandbox: pmt_sandbox::Sandbox,
}

/// How DevTools is reached: `--remote-debugging-pipe`, so the browser
/// listens on no port at all and a browser in its own network namespace
/// or `AppContainer` stays reachable.
enum Connect {
    Pipe(Arc<Cdp>),
}

impl Connect {
    async fn ready(
        &self,
        child: &mut Child,
        _profile_dir: &Path,
        timeout: Duration,
    ) -> Result<Arc<Cdp>, String> {
        let Self::Pipe(cdp) = self;
        let (cdp, probe) = (Arc::clone(cdp), Arc::clone(cdp));
        let answer = tokio::select! {
            answer = probe.call_within("Browser.getVersion", serde_json::json!({}), None, timeout) => answer,
            status = child.wait() => return Err(exited(status)),
        };
        match answer {
            Ok(_) => Ok(cdp),
            // A closed pipe is usually a browser that just exited.
            Err(why) => match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
                Ok(status) => Err(exited(status)),
                Err(_) if why.contains("timed out") => Err(format!(
                    "the browser did not start within {} s",
                    timeout.as_secs()
                )),
                Err(_) => Err(format!("the browser did not start: {why}")),
            },
        }
    }
}

/// The start failure of a browser that exited.
fn exited(status: std::io::Result<std::process::ExitStatus>) -> String {
    match status {
        Ok(status) => format!("the browser exited during start ({status})"),
        Err(err) => format!("the browser exited during start ({err})"),
    }
}

/// The flags of a browser under the OS sandbox, shared by every platform.
fn confined_flags(command: &mut std::process::Command, dirs: &Dirs, confines: bool) {
    // Chromium's own sandbox cannot start inside the OS sandbox (nested
    // Seatbelt profiles, user namespaces and AppContainers are refused);
    // the OS sandbox confines the whole process tree instead.
    if confines {
        command.arg("--no-sandbox");
    }
    // Everything the browser would write under the user's home (config,
    // caches, crash database) goes to its private temp directory instead:
    // the home is read-only in the sandbox, and Chromium aborts when its
    // crash handler has no database. (`--disable-crashpad-for-testing`
    // is no way out: Chromium's network service then crashes on start.)
    let mut crash_dir = OsString::from("--crash-dumps-dir=");
    crash_dir.push(&dirs.temp);
    command
        .env("MAC_CHROMIUM_TMPDIR", &dirs.temp)
        .env("XDG_CONFIG_HOME", &dirs.temp)
        .env("XDG_CACHE_HOME", &dirs.temp)
        .args([
            "--remote-debugging-pipe",
            "--disable-gpu",
            "--disable-dev-shm-usage",
            "--disable-crash-reporter",
            "--disable-breakpad",
        ])
        .arg(crash_dir);
}

/// Starts the confined browser command and connects DevTools over the
/// pipe; `command` holds the browser's ends of the pipes and is dropped
/// once the browser started, so the pipe reports the browser's exit.
fn start_piped(
    command: std::process::Command,
    executable: &Path,
    sandbox: pmt_sandbox::Sandbox,
    connect: impl FnOnce(&str) -> Result<Arc<Cdp>, String>,
) -> Result<Spawned, String> {
    let mut command = Command::from(command);
    command.kill_on_drop(true);
    let child = command
        .spawn()
        .map_err(|err| format!("cannot start {}: {err}", executable.display()))?;
    drop(command);
    let cdp = connect(&format!("pipe:{}", child.id().unwrap_or_default()))?;
    Ok(Spawned {
        child,
        connect: Connect::Pipe(cdp),
        sandbox,
    })
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "used as `map_err(pipe_error)`"
)]
fn pipe_error(err: std::io::Error) -> String {
    format!("cannot create the DevTools pipe: {err}")
}

#[cfg(unix)]
fn spawn(run: &Run<'_>) -> Result<Spawned, String> {
    let (executable, dirs) = (run.executable, run.dirs);
    let sandbox = super::confine::sandbox(run, &dirs.profile, &dirs.temp)?;
    let mut command = sandbox
        .command(executable)
        .map_err(|err| err.message().to_owned())?;
    base_command(&mut command, run);
    confined_flags(&mut command, dirs, sandbox.confines());
    command.arg("about:blank");
    let (to_browser_read, to_browser_write) = std::io::pipe().map_err(pipe_error)?;
    let (from_browser_read, from_browser_write) = std::io::pipe().map_err(pipe_error)?;
    pmt_sandbox::fds::pass(
        &mut command,
        vec![(to_browser_read.into(), 3), (from_browser_write.into(), 4)],
    )
    .map_err(pipe_error)?;
    start_piped(command, executable, sandbox, |label| {
        Cdp::over_pipe(label, to_browser_write.into(), from_browser_read.into())
    })
}

/// Windows: the browser runs in an `AppContainer` (which reaches no
/// loopback port of this process, so DevTools cannot listen there) and
/// inherits the pipe ends, whose handle values its command line names.
#[cfg(windows)]
fn spawn(run: &Run<'_>) -> Result<Spawned, String> {
    use std::os::windows::io::{AsHandle as _, AsRawHandle as _};

    let (executable, dirs) = (run.executable, run.dirs);
    let sandbox = super::confine::sandbox(run, &dirs.profile, &dirs.temp)?;
    let (to_browser_read, to_browser_write) = std::io::pipe().map_err(pipe_error)?;
    let (from_browser_read, from_browser_write) = std::io::pipe().map_err(pipe_error)?;
    let mut command = sandbox
        .command_inheriting(
            executable,
            &[to_browser_read.as_handle(), from_browser_write.as_handle()],
        )
        .map_err(|err| err.message().to_owned())?;
    base_command(&mut command, run);
    confined_flags(&mut command, dirs, sandbox.confines());
    command
        .arg(format!(
            "--remote-debugging-io-pipes={},{}",
            to_browser_read.as_raw_handle() as usize,
            from_browser_write.as_raw_handle() as usize
        ))
        .arg("about:blank");
    let spawned = start_piped(command, executable, sandbox, |label| {
        Cdp::over_pipe(label, to_browser_write, from_browser_read)
    });
    // The child holds its own copies now.
    drop((to_browser_read, from_browser_write));
    spawned
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
