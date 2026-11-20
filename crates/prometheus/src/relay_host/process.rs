//! The Relay host process: its managed environment, the feed that queues
//! the sessions' operations, and the thread that writes them to its stdin.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_obs::relay::feed::{self, Feed};
use pmt_pybridge::env;
use pmt_pybridge::runtime::{self, Uv};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

/// The `NeMo` Relay release the host is pinned to.
pub const REQUIREMENT: &str = "nemo-relay>=0.11,<0.12";

/// Operations queued while the host starts or falls behind; more are
/// dropped (counted), never waited for.
const QUEUE: usize = 4096;

/// How long the process's exit waits for the host to drain (first use
/// includes building its environment).
const DRAIN: Duration = Duration::from_secs(30);

/// The feed sessions write to.
#[derive(Debug)]
struct Pipe {
    queue: Mutex<Option<SyncSender<String>>>,
    capture: bool,
    dropped: AtomicU64,
}

impl Feed for Pipe {
    fn send(&self, op: Value) {
        let queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(queue) = queue.as_ref() else {
            return;
        };
        if let Err(TrySendError::Full(_)) = queue.try_send(op.to_string())
            && self.dropped.fetch_add(1, Ordering::Relaxed) == 0
        {
            tracing::warn!("NeMo Relay falls behind: events are dropped");
        }
    }

    fn captures_content(&self) -> bool {
        self.capture
    }
}

/// The running host: its feed and the signal that it drained.
struct Running {
    pipe: Arc<Pipe>,
    done: Receiver<()>,
}

fn running() -> &'static Mutex<Option<Running>> {
    static RUNNING: Mutex<Option<Running>> = Mutex::new(None);
    &RUNNING
}

/// What starting the host needs.
struct Setup {
    scope: ProfileScope,
    plugins_toml: Option<PathBuf>,
}

/// Starts the host when `scope` enables Relay (once per process) and
/// installs its feed for the sessions opened from now on.
pub fn start(scope: &ProfileScope, launch: &LaunchEnv) {
    let observability = &scope.config().observability;
    if !observability.nemo_relay.enabled {
        return;
    }
    let mut running = running().lock().unwrap_or_else(PoisonError::into_inner);
    if running.is_some() {
        return;
    }
    let (queue, ops) = sync_channel(QUEUE);
    let pipe = Arc::new(Pipe {
        queue: Mutex::new(Some(queue)),
        capture: observability.otel.capture_content,
        dropped: AtomicU64::new(0),
    });
    let setup = Setup {
        scope: scope.clone(),
        plugins_toml: super::configured(scope, launch),
    };
    let (finished, done) = sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("relay-host".to_owned())
        .spawn(move || {
            if let Err(err) = serve(&setup, &ops) {
                tracing::error!("NeMo Relay disabled: {err}");
            }
            let _ = finished.send(());
        });
    if let Err(err) = spawned {
        tracing::error!("NeMo Relay disabled: cannot start its thread: {err}");
        return;
    }
    feed::install(Some(Arc::clone(&pipe) as Arc<dyn Feed>));
    *running = Some(Running { pipe, done });
}

/// Stops feeding the host and waits (bounded) for it to send what it was
/// given; the process calls it before exiting.
pub fn stop() {
    let Some(running) = running()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    else {
        return;
    };
    feed::install(None);
    running
        .pipe
        .queue
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if running.done.recv_timeout(DRAIN).is_err() {
        tracing::warn!("NeMo Relay did not drain within {} s", DRAIN.as_secs());
    }
}

/// On the first operation (a process whose sessions run elsewhere, such
/// as a chat attached to the daemon, never starts one), builds the
/// environment, starts the host and writes every operation to it until
/// the queue closes.
fn serve(setup: &Setup, ops: &Receiver<String>) -> Result<(), Error> {
    let Ok(first) = ops.recv() else {
        return Ok(());
    };
    let python = environment(&setup.scope)?;
    let shim = pmt_pybridge::shim::materialize(&setup.scope.dir().join("python").join("shim"))?;
    let (mut child, mut stdin) = launch(&python, &shim, setup.plugins_toml.as_deref())?;
    for op in std::iter::once(first).chain(ops) {
        if let Err(err) = writeln!(stdin, "{op}") {
            tracing::error!("NeMo Relay host stopped taking events: {err}");
            break;
        }
    }
    // End of input: the host closes open scopes and flushes.
    drop(stdin);
    let _ = child.wait();
    Ok(())
}

/// The interpreter of the host's environment, built when missing.
fn environment(scope: &ProfileScope) -> Result<PathBuf, Error> {
    let home = scope.home();
    let version = scope.config().python_bridge.python_version;
    let python = runtime::require(home, version)?;
    let uv = Uv::of(home).ok_or_else(|| runtime::missing(version))?;
    let root = scope.dir().join("python").join("relay");
    let key = pmt_pybridge::hex(&Sha256::digest(
        format!("{}\n{REQUIREMENT}", python.version).as_bytes(),
    ));
    let lock_dir = root.join(".locks").join(format!("relay-{}", &key[..12]));
    let lock = lock_dir.join(env::LOCK_FILE);
    if !lock.is_file() {
        env::lock(&uv, &python, &[REQUIREMENT.to_owned()], &lock_dir)?;
    }
    Ok(env::ensure(&uv, &python, &root, "relay", &lock, false)?.python)
}

/// Starts `relay_host.py`, hands it the Relay configuration and waits for
/// its answer.
fn launch(
    python: &Path,
    shim: &Path,
    plugins_toml: Option<&Path>,
) -> Result<(Child, ChildStdin), Error> {
    let failed = |why: String| Error::new(&codes::BRIDGE_ENVIRONMENT, why);
    let mut child = Command::new(python)
        .arg("-I")
        .arg(shim.join(pmt_pybridge::shim::RELAY_HOST))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| failed(format!("cannot start the Relay host: {e}")))?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                tracing::warn!("NeMo Relay: {line}");
            }
        });
    }
    let start = serde_json::json!({"op": "start", "plugins_toml": plugins_toml});
    writeln!(stdin, "{start}").map_err(|e| failed(format!("the Relay host exited: {e}")))?;
    let mut answer = String::new();
    BufReader::new(stdout)
        .read_line(&mut answer)
        .map_err(|e| failed(format!("the Relay host did not answer: {e}")))?;
    let answer: Value = serde_json::from_str(&answer).unwrap_or_default();
    if answer["ok"] == true {
        return Ok((child, stdin));
    }
    let _ = child.kill();
    let _ = child.wait();
    Err(failed(format!(
        "NeMo Relay did not start: {}",
        answer["error"].as_str().unwrap_or("no answer")
    )))
}
