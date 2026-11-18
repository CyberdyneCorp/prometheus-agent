//! The bot screen (spec: execution-environments, Bot screen; Bot screen
//! takeover lease): a per-profile virtual desktop (Xvnc plus a minimal
//! Xfce session) the agent's headed browsers act on and the desktop app
//! streams over RFB.
//!
//! The screen runs as its own supervisor process (`prometheus
//! computer-use screen serve`), started by `screen start` or by the first
//! headed browser launch when `bot_desktop.auto_start` is on, and stopped
//! by `screen stop` or after `bot_desktop.idle_stop_secs` without use. Its files live in
//! `<profile>/run/screen/` (0700): the record `screen.json` and the `used`
//! marker whose modification time is the last use. Its sockets, the
//! control socket and the RFB socket (Xvnc serves RFB on a Unix socket
//! only, mode 0600, never on TCP), live in a short per-profile directory
//! of the system temp (0700, owned by this user; a socket path has to stay
//! under about 100 bytes). Only the profile's OS user reaches any of them;
//! the takeover lease ([`pmt_config::screen_lease`]) is a tool-level fence
//! on top.

pub mod install;
pub mod memory;
pub mod placement;
#[cfg(unix)]
pub mod serve;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The line the supervisor prints once the screen serves.
pub const READY: &str = "bot-screen-ready";

/// How long a stop may take before the supervisor is killed.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// What `screen.json` records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The supervisor process.
    pub pid: u32,
    /// The X display (`:51`).
    pub display: String,
    /// The RFB socket.
    pub rfb: PathBuf,
    /// `<width>x<height>`.
    pub geometry: String,
    /// `host`.
    pub placement: String,
    /// RFC 3339 start time.
    pub started_at: String,
}

/// `<profile>/run/screen`.
#[must_use]
pub fn dir(scope: &ProfileScope) -> PathBuf {
    scope.path("run").join("screen")
}

fn record_path(scope: &ProfileScope) -> PathBuf {
    dir(scope).join("screen.json")
}

/// The profile's socket directory: `<temp>/pmt-screen-<hash of the
/// profile directory>`.
#[must_use]
pub fn socket_dir(scope: &ProfileScope) -> PathBuf {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(scope.dir().as_os_str().as_encoded_bytes());
    let short = digest[..6].iter().fold(String::new(), |mut out, b| {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
        out
    });
    std::env::temp_dir().join(format!("pmt-screen-{short}"))
}

fn control_path(scope: &ProfileScope) -> PathBuf {
    socket_dir(scope).join("control.sock")
}

/// The marker whose modification time is the screen's last use.
#[must_use]
pub fn used_path(scope: &ProfileScope) -> PathBuf {
    dir(scope).join("used")
}

/// The persistent browser profile the bot's headed browser and the
/// desktop's browser launcher share while the screen runs.
#[must_use]
pub fn browser_profile(scope: &ProfileScope) -> PathBuf {
    scope.path("cache/browser/screen-profile")
}

/// The recorded screen, when there is one.
#[must_use]
pub fn record(scope: &ProfileScope) -> Option<Record> {
    let text = pmt_core::private_fs::read_no_follow(&record_path(scope)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Marks the screen as used now (the idle stop counts from here).
pub fn touch(scope: &ProfileScope) {
    pmt_config::screen_lease::mark_screen_used(scope.dir());
}

/// Sends `op` to the running supervisor; its JSON answer.
///
/// # Errors
///
/// Why it did not answer (not running).
#[cfg(unix)]
pub fn ask(scope: &ProfileScope, op: &str) -> Result<Value, String> {
    use std::io::{BufRead as _, BufReader, Write as _};
    let mut stream =
        std::os::unix::net::UnixStream::connect(control_path(scope)).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(format!("{}\n", serde_json::json!({"op": op})).as_bytes())
        .map_err(|e| e.to_string())?;
    let mut answer = String::new();
    BufReader::new(stream)
        .read_line(&mut answer)
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&answer).map_err(|_| "no answer".to_owned())
}

/// Elsewhere there is no screen to ask.
///
/// # Errors
///
/// Always.
#[cfg(not(unix))]
pub fn ask(_scope: &ProfileScope, _op: &str) -> Result<Value, String> {
    Err("the bot screen needs a Unix host".to_owned())
}

/// The running screen and its status, when one answers.
#[must_use]
pub fn running(scope: &ProfileScope) -> Option<(Record, Value)> {
    let record = record(scope)?;
    let status = ask(scope, "status").ok()?;
    (status["pid"].as_u64() == Some(u64::from(record.pid))).then_some((record, status))
}

/// Starts the screen of `scope` (or returns the running one).
///
/// # Errors
///
/// `screen.unavailable`: the placement, the memory floor, a host without
/// Xvnc or Xfce, or a supervisor that exited before serving.
pub fn start(scope: &ProfileScope) -> Result<Record, Error> {
    if let Some((record, _)) = running(scope) {
        return Ok(record);
    }
    let config = scope.config();
    let desktop = &config.bot_desktop;
    match placement::resolve(desktop, &config.terminal.backend)? {
        placement::Where::Host => {}
        placement::Where::Backend(hosting) => return Err(in_backend(hosting)),
    }
    if cfg!(not(unix)) {
        return Err(unavailable(
            "the bot screen needs a Linux (or other Unix) host",
        ));
    }
    memory::check(memory::free(), desktop.min_free_memory_bytes)?;
    let path = std::env::var_os("PATH");
    for program in ["Xvnc", "xfce4-session"] {
        if install::find(program, path.as_deref()).is_none() {
            return Err(unavailable(&format!("{program} is not installed"))
                .with_hint("prometheus computer-use screen install"));
        }
    }
    spawn(scope)
}

/// Starts the supervisor and waits for its ready line, or its exit.
fn spawn(scope: &ProfileScope) -> Result<Record, Error> {
    use std::io::BufRead as _;
    let exe = std::env::current_exe().map_err(|e| unavailable(&e.to_string()))?;
    let mut child = std::process::Command::new(exe)
        .arg("--home")
        .arg(scope.home())
        .args([
            "-p",
            scope.name().as_str(),
            "computer-use",
            "screen",
            "serve",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| unavailable(&e.to_string()))?;
    let stderr = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut pipe, &mut text);
            text
        })
    });
    let ready = child.stdout.take().is_some_and(|stdout| {
        std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .any(|line| line.trim() == READY)
    });
    if ready && let Some(record) = record(scope) {
        // The supervisor outlives this command.
        drop(child);
        return Ok(record);
    }
    let _ = child.kill();
    let _ = child.wait();
    let why = stderr.and_then(|r| r.join().ok()).unwrap_or_default();
    let why = why.trim().trim_start_matches("error: ");
    Err(Error::new(
        &codes::SCREEN_UNAVAILABLE,
        if why.is_empty() {
            "the bot screen exited before serving".to_owned()
        } else {
            why.to_owned()
        },
    ))
}

/// Stops the screen; `force` also clears a person's takeover lease.
/// Whether one was running.
///
/// # Errors
///
/// `screen.human_has_control` when a person holds the screen and `force`
/// is off.
pub fn stop(scope: &ProfileScope, force: bool) -> Result<bool, Error> {
    let lease = pmt_config::screen_lease::ScreenLease::of(scope.dir());
    let now = pmt_config::screen_lease::now();
    if force {
        lease.hand_back("", true, now)?.ok();
    } else if let Some(holder) = lease.read(now).holder {
        return Err(pmt_config::screen_lease::human_has_control(&holder.viewer)
            .with_hint("prometheus computer-use screen stop --force clears the takeover"));
    }
    let Some((record, _)) = running(scope) else {
        remove_files(scope);
        return Ok(false);
    };
    let _ = ask(scope, "stop");
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline {
        if ask(scope, "status").is_err() {
            remove_files(scope);
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // It answered a moment ago over its own socket: the pid is ours.
    kill(record.pid);
    remove_files(scope);
    Ok(true)
}

#[cfg(unix)]
fn kill(pid: u32) {
    pmt_tools::browser::cleanup::kill_group(pid);
}

#[cfg(not(unix))]
fn kill(_pid: u32) {}

fn remove_files(scope: &ProfileScope) {
    for path in [record_path(scope), control_path(scope)] {
        let _ = std::fs::remove_file(path);
    }
}

fn unavailable(why: &str) -> Error {
    Error::new(
        &codes::SCREEN_UNAVAILABLE,
        format!("the bot screen cannot start: {why}"),
    )
}

/// A screen placed inside the terminal backend: not built yet (the
/// container's desktop image and the SSH host's display are a known gap),
/// so it is refused with the opt-in that works.
fn in_backend(hosting: placement::Hosting) -> Error {
    let backend = match hosting {
        placement::Hosting::Container => "container",
        placement::Hosting::Ssh => "ssh",
    };
    unavailable(&format!(
        "running it inside the {backend} backend is not available in this build"
    ))
    .with_hint("set bot_desktop.placement = \"gateway\" to run it on the gateway host (outside the terminal sandbox)")
}

/// The display a headed browser of `scope` uses: the running screen's, or
/// a newly started one when `bot_desktop.auto_start` is on; the screen
/// counts as used.
#[must_use]
pub fn for_headed_browser(scope: &ProfileScope) -> Option<Record> {
    let record = match running(scope) {
        Some((record, _)) => record,
        None if scope.config().bot_desktop.auto_start => start(scope)
            .inspect_err(|e| tracing::warn!("bot screen auto start: {}", e.message()))
            .ok()?,
        None => return None,
    };
    touch(scope);
    Some(record)
}

/// Idle seconds since the last use.
#[must_use]
pub fn idle_secs(scope: &ProfileScope) -> Option<u64> {
    let modified = std::fs::metadata(used_path(scope)).ok()?.modified().ok()?;
    Some(modified.elapsed().map_or(0, |d| d.as_secs()))
}
