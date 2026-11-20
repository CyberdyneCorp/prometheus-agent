//! The profile's egress proxy for container and remote sandboxes (spec:
//! security-boundaries, Egress proxy operation and status; Egress proxy
//! coverage of containers and remote sandboxes; Egress credential source
//! and placeholder rotation).
//!
//! It runs as its own process (`prometheus egress serve`), started by the
//! gateway daemon for each served profile with credential injection on
//! (and stopped with it), or by `prometheus egress start`. It records
//! itself in `<profile>/run/egress-proxy.json`, created exclusively (never
//! through a link) with a per-start nonce: every command talks to it over
//! its loopback control port presenting that nonce, so a recycled pid is
//! never signalled. It serves a loopback listener and the container bridge
//! listener (`security.egress.proxy_port`), injects the mapped secrets of
//! [`super::mappings`] and always refuses private and metadata addresses
//! upstream.

pub mod bridge;
pub mod cover;
pub mod serve;

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// How long the proxy retries a container listener port still in use (a
/// proxy that just stopped) before the start fails.
pub const BIND_TIMEOUT: Duration = Duration::from_secs(5);

/// The line the proxy prints on its stdout once it serves and its record
/// is written.
pub const READY: &str = "egress-proxy-ready";

/// How long a graceful stop may take before the process is killed.
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// Where the CA bundle and certificate appear inside a container.
pub const CONTAINER_BUNDLE: &str = "/etc/prometheus/egress/ca-bundle.pem";
/// The egress CA certificate inside a container.
pub const CONTAINER_CA: &str = "/etc/prometheus/egress/ca.crt";

/// What `egress-proxy.json` records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// The proxy process.
    pub pid: u32,
    /// This start's nonce: the control channel's credential.
    pub nonce: String,
    /// RFC 3339 start time.
    pub started_at: String,
    /// The control listener (loopback).
    pub control: SocketAddr,
    /// The loopback proxy listener.
    pub loopback: SocketAddr,
    /// The container bridge listener.
    pub bridge: SocketAddr,
    /// The address the relay maps the host alias to (rootless runtimes;
    /// see [`bridge`]).
    #[serde(default)]
    pub relay_host: Option<std::net::IpAddr>,
}

/// The record file of `scope`.
#[must_use]
pub fn record_path(scope: &ProfileScope) -> PathBuf {
    scope.path("run").join("egress-proxy.json")
}

/// The record on disk, when there is one.
#[must_use]
pub fn record(scope: &ProfileScope) -> Option<Record> {
    let text = pmt_core::private_fs::read_no_follow(&record_path(scope)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Sends `op` to the proxy of `record`; its JSON answer.
///
/// # Errors
///
/// Why the proxy did not answer (not running, wrong nonce).
pub fn ask(record: &Record, op: &str) -> Result<Value, String> {
    let mut stream = TcpStream::connect_timeout(&record.control, Duration::from_secs(2))
        .map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let line = json!({"nonce": record.nonce, "op": op}).to_string();
    stream
        .write_all(format!("{line}\n").as_bytes())
        .map_err(|e| e.to_string())?;
    let mut answer = String::new();
    BufReader::new(stream)
        .read_line(&mut answer)
        .map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_str(&answer).map_err(|_| "no answer".to_owned())?;
    match value["error"].as_str() {
        Some(error) => Err(error.to_owned()),
        None => Ok(value),
    }
}

/// The running proxy of `scope` with its status, when one answers.
#[must_use]
pub fn running(scope: &ProfileScope) -> Option<(Record, Value)> {
    let record = record(scope)?;
    let status = ask(&record, "status").ok()?;
    (status["pid"].as_u64() == Some(u64::from(record.pid))).then_some((record, status))
}

/// The daemon owns the egress proxy of a profile that injects
/// credentials (spec: security-boundaries, Egress proxy operation and
/// status): it ends when the daemon drops the child (its stdin closes).
#[must_use]
pub fn own(scope: &ProfileScope) -> Option<std::process::Child> {
    if !covers(scope) || running(scope).is_some() {
        return None;
    }
    match start(scope, true) {
        Ok((_, child)) => Some(child),
        Err(err) => {
            tracing::error!(profile = %scope.name(), "{}", err.message());
            None
        }
    }
}

/// Starts the proxy of `scope`; `owned`: it ends when the returned child's
/// stdin closes (the daemon keeps it).
///
/// It returns once the proxy says it serves ([`READY`] on its stdout) or
/// has exited. There is no wall-clock deadline here: the proxy's slow
/// steps (the container runtime probe, a Bitwarden fetch) are bounded by
/// their own call timeouts, so a loaded host delays the start instead of
/// failing it.
///
/// # Errors
///
/// `egress.proxy_start` when it is already running or exited before
/// serving (its own error is included).
pub fn start(scope: &ProfileScope, owned: bool) -> Result<(Record, std::process::Child), Error> {
    if let Some((record, _)) = running(scope) {
        return Err(start_error(&format!(
            "the egress proxy of profile '{}' is already running (pid {})",
            scope.name(),
            record.pid
        )));
    }
    let exe = std::env::current_exe().map_err(|e| start_error(&e.to_string()))?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("--home")
        .arg(scope.home())
        .args(["-p", scope.name().as_str(), "egress", "serve"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if owned {
        command.arg("--owned").stdin(std::process::Stdio::piped());
    } else {
        command.stdin(std::process::Stdio::null());
    }
    let mut child = command.spawn().map_err(|e| start_error(&e.to_string()))?;
    // Drained all along: a proxy that logs a lot must never block on a
    // full pipe while we wait for its ready line.
    let stderr = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut pipe, &mut text);
            text
        })
    });
    // Until the ready line, or end of file: the proxy exited.
    let ready = child.stdout.take().is_some_and(|stdout| {
        BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .any(|line| line.trim() == READY)
    });
    if ready
        && let Some(record) = record(scope)
        && record.pid == child.id()
    {
        return Ok((record, child));
    }
    let _ = child.kill();
    let _ = child.wait();
    let why = stderr
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    let why = why.trim().trim_start_matches("error: ");
    let why = why
        .strip_prefix("the egress proxy did not start: ")
        .unwrap_or(why);
    Err(start_error(if why.is_empty() {
        "it exited before serving"
    } else {
        why
    }))
}

/// Stops the proxy of `scope` (graceful, killed after [`STOP_GRACE`]);
/// whether one was running. The container relay to it goes too.
///
/// # Errors
///
/// `home.io` when its record cannot be removed.
pub fn stop(scope: &ProfileScope) -> Result<bool, Error> {
    cover::remove_relay(scope);
    let Some((record, _)) = running(scope) else {
        // A record nobody answers for is stale: never signal its pid.
        remove_record(scope)?;
        return Ok(false);
    };
    let _ = ask(&record, "stop");
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline {
        if ask(&record, "status").is_err() {
            remove_record(scope)?;
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // It answered with this start's nonce a moment ago: the pid is ours.
    kill(record.pid);
    remove_record(scope)?;
    Ok(true)
}

#[cfg(unix)]
fn kill(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

#[cfg(not(unix))]
fn kill(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .output();
}

fn remove_record(scope: &ProfileScope) -> Result<(), Error> {
    let path = record_path(scope);
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(Error::io(&codes::HOME_IO, &path, &e))
        }
        _ => Ok(()),
    }
}

fn start_error(why: &str) -> Error {
    Error::new(
        &codes::EGRESS_PROXY_START,
        format!("the egress proxy did not start: {why}"),
    )
}

/// Whether container and remote sandboxes of `scope` go through the
/// egress proxy: credential injection is on and maps something.
#[must_use]
pub fn covers(scope: &ProfileScope) -> bool {
    let egress = &scope.config().security.egress;
    egress.credential_injection && !egress.inject.is_empty()
}

/// Refuses a container or remote sandbox of `scope` while injection is on,
/// enforced, and the proxy is down (checked before anything is started).
///
/// # Errors
///
/// `egress.proxy_down`.
pub fn require_running(scope: &ProfileScope) -> Result<(), Error> {
    if covers(scope)
        && scope.config().security.egress.enforce_on_container
        && running(scope).is_none()
    {
        return Err(down());
    }
    Ok(())
}

/// The refusal of a sandbox start without the proxy.
#[must_use]
pub fn down() -> Error {
    Error::new(&codes::EGRESS_PROXY_DOWN, "egress proxy is not running")
        .with_hint("start it with `prometheus egress start` (the daemon starts it too)")
}
