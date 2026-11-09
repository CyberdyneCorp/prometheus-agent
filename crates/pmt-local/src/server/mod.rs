//! The llama-server behind the `llamacpp` provider (spec: local-models,
//! Managed server lifecycle; External llama-server detection; Local
//! runtime errors).
//!
//! [`Runtime::endpoint`] answers where a `llamacpp` route without a
//! `base_url` sends its requests, in this order:
//!
//! 1. a llama-server someone runs on `127.0.0.1:8080` or a port of
//!    `local_runtime.detect_ports`;
//! 2. the managed server another process of this home recorded in
//!    `run/llamacpp.json`;
//! 3. with `local_runtime.enabled`, a managed server started now: by the
//!    gateway daemon when one runs (the [`Delegate`] asks it), else by a
//!    helper process (`prometheus local supervise`) that supervises it and
//!    stops it when this process exits (it watches its stdin, which only
//!    this process holds open);
//!
//! and otherwise fails with `provider.local_runtime_down`. Nothing here
//! downloads: the engine and models must already be installed.

pub mod detect;
pub mod launch;
pub mod record;
pub mod supervise;

use std::future::Future;
use std::io::BufRead as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use pmt_config::daemon::DaemonConfig;
use pmt_config::daemon_local::{LocalBackend, LocalRuntimeConfig};
use pmt_core::error::{Error, codes};
use serde::Serialize;

use crate::download::io_error;
use crate::engine::{Engine, Installed};
use crate::library::Library;
pub use launch::Launch;
pub use record::ServerRecord;
pub use supervise::Stop;

/// The first line of the failure, as the spec words it.
pub const DOWN: &str = "the local model server isn't running";

/// Builds the helper command that runs [`serve`] in its own process.
pub type HelperCommand = Arc<dyn Fn() -> Command + Send + Sync>;

/// Asks the gateway daemon of the home to start and supervise the managed
/// server; resolves to `true` once it runs under the daemon, `false` when
/// no daemon (other than this process) can take it.
pub type Delegate = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

/// Helpers this process started; their stdin stays open for the life of
/// the process, so they stop their server once it exits.
static HELPERS: Mutex<Vec<Child>> = Mutex::new(Vec::new());

/// Where requests go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Endpoint {
    /// OpenAI-compatible base URL.
    pub base_url: String,
    /// `external`, `managed` (recorded by another process) or `started`.
    pub source: &'static str,
}

/// The local runtime of one home.
#[derive(Clone)]
pub struct Runtime {
    home: PathBuf,
    helper: Option<HelperCommand>,
    delegate: Option<Delegate>,
    client: Arc<pmt_net::LazyClient>,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

impl Runtime {
    /// The runtime of `home`; `helper` starts a supervising process (None:
    /// this process cannot start a server).
    #[must_use]
    pub fn new(home: &Path, helper: Option<HelperCommand>) -> Self {
        Self {
            home: home.to_owned(),
            helper,
            delegate: None,
            client: Arc::new(detect::lazy_loopback_client()),
        }
    }

    /// Hands a server this runtime would start to the daemon first (spec:
    /// the gateway daemon supervises it when it runs); the helper only
    /// starts one when the delegate declines.
    #[must_use]
    pub fn with_delegate(mut self, delegate: Delegate) -> Self {
        self.delegate = Some(delegate);
        self
    }

    /// The endpoint of the local model server (see the module doc).
    ///
    /// # Errors
    ///
    /// `provider.local_runtime_down` (69) naming the remedy; `config.parse`
    /// for a bad `daemon.toml`.
    pub async fn endpoint(&self) -> Result<Endpoint, Error> {
        let config = DaemonConfig::load(&self.home)?.local_runtime;
        let ports = detect::ports(&config.detect_ports);
        if let Some(port) = detect::first(self.client.get()?, &ports).await {
            return Ok(Endpoint {
                base_url: record::base_url(port),
                source: "external",
            });
        }
        if let Some(found) = recorded(&self.home) {
            return Ok(Endpoint {
                base_url: found.base_url(),
                source: "managed",
            });
        }
        if !config.enabled {
            return Err(down(
                &config,
                "local_runtime.enabled is false and no server was detected",
            ));
        }
        self.start_managed(&config).await
    }

    /// Starts the managed server with this process as its supervisor
    /// (`prometheus local start` in the daemon), whatever
    /// `local_runtime.enabled` says; the recorded one when it runs.
    ///
    /// # Errors
    ///
    /// `provider.local_runtime_down` when the engine is not installed or
    /// the server does not start; `config.parse`.
    pub async fn start(&self) -> Result<Endpoint, Error> {
        let config = DaemonConfig::load(&self.home)?.local_runtime;
        if let Some(found) = recorded(&self.home) {
            return Ok(Endpoint {
                base_url: found.base_url(),
                source: "managed",
            });
        }
        self.start_managed(&config).await
    }

    async fn start_managed(&self, config: &LocalRuntimeConfig) -> Result<Endpoint, Error> {
        let Some(engine) = pick_engine(&self.home, config) else {
            return Err(down(config, "the llama.cpp engine is not installed"));
        };
        if let Some(delegate) = &self.delegate
            && delegate().await
            && let Some(found) = recorded(&self.home)
        {
            return Ok(Endpoint {
                base_url: found.base_url(),
                source: "managed",
            });
        }
        let Some(helper) = self.helper.clone() else {
            return Err(down(config, "this process cannot start the managed server"));
        };
        let home = self.home.clone();
        let started = tokio::task::spawn_blocking(move || start(&home, &helper, &engine))
            .await
            .map_err(|e| {
                Error::new(&codes::PROVIDER_LOCAL_RUNTIME_DOWN, format!("{DOWN}: {e}"))
            })??;
        Ok(Endpoint {
            base_url: started.base_url(),
            source: "started",
        })
    }
}

/// The recorded managed server, when it runs and answers.
#[must_use]
pub fn recorded(home: &Path) -> Option<ServerRecord> {
    record::live(home).filter(|r| supervise::health(r.port))
}

/// The installed build of the pinned version to run: the configured
/// backend's, or under `auto` the installed one (a GPU build before the
/// CPU build).
#[must_use]
pub fn pick_engine(home: &Path, config: &LocalRuntimeConfig) -> Option<Installed> {
    let engine = Engine::open(home);
    if config.backend != LocalBackend::Auto {
        return engine.installed(config.backend);
    }
    let pinned = engine.index().version.clone();
    let mut builds: Vec<Installed> = engine
        .installed_all()
        .into_iter()
        .filter(|b| b.record.version == pinned)
        .collect();
    builds.sort_by_key(|b| b.record.backend == LocalBackend::Cpu);
    builds.into_iter().next()
}

/// The `provider.local_runtime_down` error: the spec's sentence, why, and
/// every remedy.
#[must_use]
pub fn down(config: &LocalRuntimeConfig, why: &str) -> Error {
    let ports = detect::ports(&config.detect_ports)
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    Error::new(&codes::PROVIDER_LOCAL_RUNTIME_DOWN, format!("{DOWN} ({why})")).with_hint(format!(
        "run 'prometheus local start', or set local_runtime.enabled = true in daemon.toml (after 'prometheus local install'), or run llama-server on 127.0.0.1 port {ports} (local_runtime.detect_ports adds ports)"
    ))
}

/// Starts a supervising helper unless another process won the race; the
/// cross-process lock makes the check-and-start atomic.
fn start(home: &Path, helper: &HelperCommand, engine: &Installed) -> Result<ServerRecord, Error> {
    let lock_path = pmt_core::runtime::run_dir(home).join("llamacpp.lock");
    if let Some(dir) = lock_path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io_error(dir, &e))?;
    }
    let lock = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| io_error(&lock_path, &e))?;
    lock.lock().map_err(|e| io_error(&lock_path, &e))?;
    if let Some(found) = recorded(home) {
        return Ok(found);
    }
    let mut child = helper()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| {
            Error::new(
                &codes::PROVIDER_LOCAL_RUNTIME_DOWN,
                format!("{DOWN}: cannot start its supervisor: {e}"),
            )
        })?;
    let ready = child.stdout.take().and_then(|out| {
        std::io::BufReader::new(out)
            .lines()
            .map_while(Result::ok)
            .find(|l| l.starts_with("ready "))
    });
    HELPERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(child);
    match ready.and_then(|_| record::live(home)) {
        Some(found) => Ok(found),
        None => Err(Error::new(
            &codes::PROVIDER_LOCAL_RUNTIME_DOWN,
            format!("{DOWN}: {} did not start", engine.exe().display()),
        )
        .with_hint(format!("see {}", supervise::log_path(home).display()))),
    }
}

/// Runs and supervises the managed server of `home` in this process until
/// `stop` is raised; prints nothing itself, `ready` hears each start.
///
/// # Errors
///
/// `provider.local_runtime_down` when the engine is missing or the server
/// keeps failing; `config.parse`; `runtime.io`.
pub fn serve(home: &Path, stop: &Stop, ready: &mut dyn FnMut(&ServerRecord)) -> Result<(), Error> {
    let config = DaemonConfig::load(home)?.local_runtime;
    let engine = pick_engine(home, &config).ok_or_else(|| {
        down(&config, "the llama.cpp engine is not installed")
            .with_hint("run 'prometheus local install'")
    })?;
    let models = Library::open(home).models();
    launch::write_preset(home, &models, engine.record.backend)?;
    let port = match config.port {
        0 => launch::free_port()?,
        port => port,
    };
    let launch = Launch::new(home, &engine, &config, port);
    supervise::supervise(home, &launch, stop, ready)
}

/// After the library changed: rewrites the preset and asks a running
/// managed server to reload its model list.
///
/// # Errors
///
/// `runtime.io` when the preset cannot be written; `net.client_init`.
pub async fn refresh(home: &Path) -> Result<(), Error> {
    let Some(found) = record::live(home) else {
        return Ok(());
    };
    launch::write_preset(home, &Library::open(home).models(), found.backend)?;
    let url = format!("http://127.0.0.1:{}/models?reload=1", found.port);
    let _ = detect::loopback_client()?.get(url).send().await;
    Ok(())
}
