//! Finding and launching a local Chromium (spec: execution-environments,
//! Browser session resolution; distribution-and-update, Lazy runtime
//! installation policy).
//!
//! The executable comes from the managed runtime store when it holds
//! `chromium`, else from a missing runtime installed on first use (when
//! `runtimes.allow_lazy_install` permits), else from a Chromium-family
//! browser on the host. Each launch gets a throwaway profile directory and
//! `--remote-debugging-port=0`; the DevTools endpoint is read back from the
//! profile's `DevToolsActivePort` file.

use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::{Child, Command};

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
    /// The spec message when lazy installs are disabled, or the install
    /// failure.
    pub async fn executable(&self, name: &str) -> Result<PathBuf, String> {
        if let Some(path) = self.runtimes.installed(name) {
            return Ok(path);
        }
        if let Some(path) = self.host_candidates.iter().find(|p| p.is_file()) {
            return Ok(path.clone());
        }
        if !self.allow_lazy_install {
            return Err(format!(
                "{name} is not installed and lazy installs are disabled: run 'prometheus runtimes install {name}'"
            ));
        }
        self.runtimes.install(name).await
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
    /// The browser-level DevTools WebSocket URL.
    pub endpoint: String,
    /// The throwaway profile directory.
    pub profile_dir: PathBuf,
}

/// Launches `executable` headless on a fresh profile directory under
/// `profiles_root` with `env` as its whole environment.
///
/// # Errors
///
/// The spawn failed, or the browser exited or never published its
/// DevTools endpoint within `timeout`.
pub async fn launch(
    executable: &Path,
    profiles_root: &Path,
    env: &ChildEnv,
    timeout: Duration,
) -> Result<Launched, String> {
    let profile_dir = profiles_root.join(ulid::Ulid::generate().to_string().to_lowercase());
    pmt_core::private_fs::create_dir(&profile_dir)
        .map_err(|err| format!("cannot create the browser profile: {err}"))?;
    let mut flag = OsString::from("--user-data-dir=");
    flag.push(&profile_dir);
    let mut std_command = std::process::Command::new(executable);
    env.apply(&mut std_command);
    std_command
        .arg("--headless=new")
        .arg("--remote-debugging-port=0")
        .arg(flag)
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-sync",
            "--password-store=basic",
            "--use-mock-keychain",
            "about:blank",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut command = Command::from(std_command);
    command.kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|err| format!("cannot start {}: {err}", executable.display()))?;
    let pid = child.id().unwrap_or_default();
    let endpoint = match wait_for_endpoint(&mut child, &profile_dir, timeout).await {
        Ok(endpoint) => endpoint,
        Err(why) => {
            let _ = child.start_kill();
            let _ = std::fs::remove_dir_all(&profile_dir);
            return Err(why);
        }
    };
    Ok(Launched {
        child,
        pid,
        endpoint,
        profile_dir,
    })
}

/// Polls `DevToolsActivePort` (port, then path) until the browser writes it.
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
