//! The Lightpanda engine (spec: execution-environments, Lightpanda engine).
//!
//! With `browser.engine = "lightpanda"` and nothing shadowing it (an
//! explicit CDP endpoint, a cloud provider, real-profile browsing), each
//! browser session gets one `lightpanda serve --host 127.0.0.1 --port
//! <free>` process, owned, recorded and reaped exactly like a local
//! Chromium. Lightpanda serves DevTools only on TCP, so its sandbox grants
//! it that one loopback port; on Linux, where the browser has a network
//! namespace of its own, the parent reaches the port through sockets the
//! browser created in that namespace ([`pmt_sandbox::Inbound`]). Its only
//! way out is the session's egress proxy, as for Chromium.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pmt_net::egress::Proxy;
use tokio::process::{Child, Command};

use super::cdp::Cdp;
use super::cleanup;
use super::confine::{self, Confinement, Target};
use super::launch::{Launched, RuntimePolicy, StderrTail, exited, temp_root};
use crate::terminal::env::ChildEnv;

/// The runtime (and executable) name.
pub const RUNTIME: &str = "lightpanda";

/// What the model is told when a session browses on Lightpanda.
pub const LIMITS: &str = "This browser runs on the Lightpanda engine: screenshots are \
    unavailable (capture_screenshot fails) and only one page is supported (work in the \
    current tab; new_tab replaces it).";

/// The Lightpanda executable: the managed runtime, else `lightpanda` on
/// `path_var`, else a lazy install when the policy allows one; `None`
/// when none is available (the session then uses Chromium).
pub async fn executable(
    policy: &RuntimePolicy,
    path_var: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if let Some(path) = policy.runtimes.installed(RUNTIME) {
        return Some(path);
    }
    if let Some(found) = path_var.and_then(|path| {
        crate::terminal::shell::find_all(RUNTIME, path, None)
            .into_iter()
            .next()
    }) {
        return Some(found);
    }
    if policy.allow_lazy_install && !policy.runtimes.excluded(RUNTIME) {
        return policy.runtimes.install(RUNTIME).await.ok();
    }
    None
}

/// A free loopback port other than `avoid` (the egress proxy's, which a
/// namespaced browser's loopback already uses).
fn free_port(avoid: u16) -> Result<u16, String> {
    for _ in 0..8 {
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .and_then(|l| l.local_addr())
            .map_err(|err| format!("cannot pick a port for Lightpanda: {err}"))?
            .port();
        if port != avoid {
            return Ok(port);
        }
    }
    Err("cannot pick a port for Lightpanda".to_owned())
}

/// Launches Lightpanda on a throwaway directory under `profiles_root`,
/// confined like a local Chromium and going out only through `proxy`.
///
/// # Errors
///
/// The sandbox cannot be built, the spawn failed, or Lightpanda exited or
/// did not answer over DevTools within `timeout`.
pub async fn launch(
    executable: &Path,
    profiles_root: &Path,
    env: &ChildEnv,
    proxy: &Proxy,
    confinement: &Confinement,
    timeout: Duration,
) -> Result<Launched, String> {
    let id = ulid::Ulid::generate().to_string().to_lowercase();
    let profile_dir = profiles_root.join(&id);
    let temp_dir = temp_root(profiles_root).join(format!("pmt-lightpanda-{}", &id[16..]));
    for dir in [&profile_dir, &temp_dir] {
        if let Err(err) = pmt_core::private_fs::create_dir(dir) {
            remove(&profile_dir, &temp_dir).await;
            return Err(format!("cannot create Lightpanda's directories: {err}"));
        }
    }
    let started = start(
        executable,
        &profile_dir,
        &temp_dir,
        env,
        proxy,
        confinement,
        timeout,
    )
    .await;
    match started {
        Ok((child, pid, cdp, sandbox)) => Ok(Launched {
            child,
            pid,
            cdp,
            profile_dir,
            keep_profile: false,
            temp_dir,
            sandbox,
        }),
        Err(why) => {
            remove(&profile_dir, &temp_dir).await;
            Err(why)
        }
    }
}

async fn remove(profile_dir: &Path, temp_dir: &Path) {
    cleanup::remove_dir_async(profile_dir).await;
    cleanup::remove_dir_async(temp_dir).await;
}

type Started = (Child, u32, Arc<Cdp>, pmt_sandbox::Sandbox);

#[allow(clippy::too_many_arguments)]
async fn start(
    executable: &Path,
    profile_dir: &Path,
    temp_dir: &Path,
    env: &ChildEnv,
    proxy: &Proxy,
    confinement: &Confinement,
    timeout: Duration,
) -> Result<Started, String> {
    let port = free_port(proxy.addr().port())?;
    let inbound = pmt_sandbox::Inbound::new();
    let target = Target {
        executable,
        profile: profile_dir,
        temp: temp_dir,
        display: None,
        listen: Some((port, inbound.clone())),
    };
    let sandbox = confine::build(&target, proxy, confinement)?;
    // Lightpanda reads the system trust store itself (the macOS keychain
    // files the sandbox denies): it gets the platform roots as a bundle.
    let bundle = temp_dir.join("ca-bundle.pem");
    pmt_net::tls::write_child_bundle(&bundle, &[])
        .map_err(|err| format!("cannot write Lightpanda's CA bundle: {}", err.message()))?;
    let mut command = sandbox
        .command(executable)
        .map_err(|err| err.message().to_owned())?;
    let mut env = env.clone();
    env.set_temp_dir(temp_dir);
    for (name, dir) in [
        ("HOME", profile_dir),
        ("XDG_CONFIG_HOME", temp_dir),
        ("XDG_CACHE_HOME", temp_dir),
    ] {
        env.set(name, dir);
    }
    env.set("LIGHTPANDA_DISABLE_TELEMETRY", "true");
    env.apply(&mut command);
    command
        .args(["serve", "--host", "127.0.0.1", "--port"])
        .arg(port.to_string())
        .arg("--http-proxy")
        .arg(proxy.url())
        .arg("--ca-cert")
        .arg(&bundle)
        .current_dir(temp_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cleanup::new_group(&mut command);
    let mut command = Command::from(command);
    command.kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|err| format!("cannot start {}: {err}", executable.display()))?;
    // The browser holds its ends of the namespace channels now: dropping
    // ours lets the parent see when it sent everything.
    drop(command);
    let pid = child.id().unwrap_or_default();
    let log = StderrTail::drain(child.stderr.take());
    match ready(&mut child, &inbound, port, pid, timeout).await {
        Ok(cdp) => Ok((child, pid, cdp, sandbox)),
        Err(why) => {
            cleanup::kill_group(pid);
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(log.explain(why).await)
        }
    }
}

/// Waits until Lightpanda answers over DevTools on `port`.
async fn ready(
    child: &mut Child,
    inbound: &pmt_sandbox::Inbound,
    port: u16,
    pid: u32,
    timeout: Duration,
) -> Result<Arc<Cdp>, String> {
    let deadline = Instant::now() + timeout;
    let mut last = String::from("no answer");
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(exited(Ok(status)));
        }
        match connect(inbound, port, pid).await {
            Ok(cdp) => {
                let left = deadline.saturating_duration_since(Instant::now());
                return match cdp
                    .call_within("Browser.getVersion", serde_json::json!({}), None, left)
                    .await
                {
                    Ok(_) => Ok(cdp),
                    Err(why) => Err(format!("Lightpanda did not answer over DevTools: {why}")),
                };
            }
            Err(why) => last = why,
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!(
        "Lightpanda did not start within {} s ({last})",
        timeout.as_secs()
    ))
}

/// One DevTools connection to Lightpanda's `port`. Its label is no URL:
/// a lost connection is a crashed browser, replaced on next use like a
/// Chromium on a pipe.
async fn connect(inbound: &pmt_sandbox::Inbound, port: u16, pid: u32) -> Result<Arc<Cdp>, String> {
    let inbound = inbound.clone();
    let stream = pmt_core::spawn::blocking(move || inbound.connect(port))
        .await
        .map_err(|err| err.to_string())?
        .map_err(|err| err.to_string())?;
    stream
        .set_nonblocking(true)
        .map_err(|err| err.to_string())?;
    let stream = tokio::net::TcpStream::from_std(stream).map_err(|err| err.to_string())?;
    let url = format!("ws://127.0.0.1:{port}/");
    let (socket, _) = tokio_tungstenite::client_async(url, stream)
        .await
        .map_err(|err| err.to_string())?;
    Ok(Cdp::over_websocket(&format!("lightpanda:{pid}"), socket))
}
