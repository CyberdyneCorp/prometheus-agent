//! A session's language-server diagnostics (spec: lsp-diagnostics,
//! Diagnostics introduced by an edit; Workspace trust; Server
//! installation).
//!
//! After `write_file` or `patch` changes a file, [`SessionDiagnostics`]
//! routes it (gate, registry, trust), starts a check on the runtime and
//! waits for the verdict only while the tool call stays under
//! [`INLINE_BUDGET`]. A slower verdict is queued for the session's next
//! tool result; before the turn's final answer is accepted the turn loop
//! [`settles`](SessionDiagnostics::settle) the pending checks. An edit of
//! a file whose previous check is still running is compared against that
//! check's baseline (the content before the first edit).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pmt_config::schema::{LspConfig, LspInstallStrategy, LspWaitMode};
use serde_json::{Map, Value, json};
use tokio::sync::{OnceCell, watch};

use super::check::{self, TimedOut};
use super::client::Start;
use super::install::{Installs, Resolved, Source, Tools};
use super::launch::{LaunchSpec, Launcher};
use super::pool::{Launch, Servers, Unavailable};
use super::protocol::{self, Diagnostic};
use super::registry::{Install, Pin, Registry, Server};
use super::workspace::{self, Gate, Workspaces};
use crate::browser::RuntimePolicy;

/// The longest a write waits for its verdict (the whole tool call).
pub const INLINE_BUDGET: Duration = Duration::from_millis(80);

/// The managed runtimes servers install and run with.
#[derive(Clone, Default)]
pub struct Toolchain {
    /// Runtime `node` (npm installs; running npm servers).
    pub node: Option<RuntimePolicy>,
    /// Runtime `cpython` (pip installs; pyright's pinned interpreter).
    pub python: Option<RuntimePolicy>,
    /// `runtimes.allow_lazy_install`: installs on first use.
    pub allow_lazy_install: bool,
}

/// What a session's diagnostics are built from.
pub struct LspSetup {
    /// `[lsp]`.
    pub config: LspConfig,
    /// The profile directory (status files, caches, broken marks).
    pub profile_dir: PathBuf,
    /// The session.
    pub session_id: String,
    /// Gating and trust, fixed at session start.
    pub workspaces: Workspaces,
    /// Server binaries.
    pub installs: Installs,
    /// Starts servers confined.
    pub launcher: Arc<dyn Launcher>,
    /// Managed runtimes.
    pub toolchain: Toolchain,
    /// Where checks run.
    pub handle: tokio::runtime::Handle,
}

/// Where an edited file's check goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// No server: not a repository, an excluded root or no server for
    /// this file type.
    None,
    /// A server exists but may not start; the log line says why.
    Skipped(String),
    /// A check of `server` for `root`.
    Check {
        /// The server id.
        server: String,
        /// The language id.
        language: String,
        /// The server's workspace root.
        root: PathBuf,
        /// Whether the workspace is trusted.
        trusted: bool,
    },
}

/// The edit chain of one file: the baseline every edit of the chain is
/// compared against, and which edit is newest.
struct Chain {
    generation: u64,
    baseline: Arc<OnceCell<Vec<Diagnostic>>>,
}

/// Where a check's verdict goes.
enum Slot {
    /// The write is still waiting.
    Waiting(mpsc::SyncSender<Vec<String>>),
    /// The write returned: the verdict goes to the next tool result.
    Late,
}

struct Inner {
    setup: LspSetup,
    registry: Registry,
    servers: Servers,
    chains: Mutex<HashMap<PathBuf, Chain>>,
    late: Mutex<Vec<String>>,
    pending: watch::Sender<usize>,
    notified: Mutex<HashSet<String>>,
}

/// A session's language-server diagnostics.
#[derive(Clone)]
pub struct SessionDiagnostics {
    inner: Arc<Inner>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl SessionDiagnostics {
    /// The diagnostics of a session, or `None` when `lsp.enabled = false`
    /// or the session's backend does not use the host filesystem: then
    /// no server, watcher or task exists.
    #[must_use]
    pub fn new(setup: LspSetup, uses_host_filesystem: bool) -> Option<Self> {
        if !setup.config.enabled || !uses_host_filesystem {
            return None;
        }
        let config = &setup.config;
        let servers = Servers::new(
            &setup.profile_dir,
            &setup.session_id,
            config.idle_timeout_secs,
            config.broken_retry_secs,
        );
        Some(Self {
            inner: Arc::new(Inner {
                registry: Registry::new(config),
                servers,
                setup,
                chains: Mutex::default(),
                late: Mutex::default(),
                pending: watch::channel(0).0,
                notified: Mutex::default(),
            }),
        })
    }

    /// Where a check of `path` goes.
    #[must_use]
    pub fn route(&self, path: &Path) -> Route {
        let inner = &self.inner;
        let Gate::Root { root, trusted } = inner.setup.workspaces.gate(path) else {
            return Route::None;
        };
        let Some((server, language)) = inner.registry.for_file(path) else {
            return Route::None;
        };
        let untrusted = || {
            format!(
                "skipped: untrusted workspace {}; add it to lsp.trusted_workspaces",
                root.display()
            )
        };
        if !trusted && server.executes_project_code {
            return Route::Skipped(untrusted());
        }
        let root = workspace::server_root(path, &root, &server.root_markers);
        Route::Check {
            server: server.id.clone(),
            language: language.to_owned(),
            root,
            trusted,
        }
    }

    /// Checks a written file: the rendered diagnostics the edit introduced
    /// when the verdict arrives within [`INLINE_BUDGET`] of `started`;
    /// otherwise the check goes on and its findings reach the next tool
    /// result. Blocks the calling (blocking) thread.
    #[must_use]
    pub fn after_write(
        &self,
        path: &Path,
        before: Option<&str>,
        after: &str,
        started: std::time::Instant,
    ) -> Option<Vec<String>> {
        let route = self.route(path);
        tracing::debug!(path = %path.display(), ?route, "language-server check");
        let Route::Check {
            server,
            language,
            root,
            trusted,
        } = route
        else {
            if let Route::Skipped(line) = route {
                self.notice_once(&line, &line);
            }
            return None;
        };
        let (generation, baseline) = self.chain(path);
        let (tx, rx) = mpsc::sync_channel(1);
        let slot = Arc::new(Mutex::new(Slot::Waiting(tx)));
        let job = Job {
            path: path.to_path_buf(),
            server,
            language,
            root,
            trusted,
            before: before.map(str::to_owned),
            after: after.to_owned(),
            generation,
            baseline,
        };
        self.inner.pending.send_modify(|n| *n += 1);
        let inner = Arc::clone(&self.inner);
        let delivered = Arc::clone(&slot);
        self.inner.setup.handle.spawn(async move {
            let verdict = inner.check(&job).await;
            inner.deliver(&job, verdict, &delivered);
            inner.pending.send_modify(|n| *n -= 1);
        });
        let budget = INLINE_BUDGET.saturating_sub(started.elapsed());
        if let Ok(lines) = rx.recv_timeout(budget) {
            return (!lines.is_empty()).then_some(lines);
        }
        let mut slot = lock(&slot);
        // The verdict may have landed between the timeout and the lock.
        if let Ok(lines) = rx.try_recv() {
            return (!lines.is_empty()).then_some(lines);
        }
        *slot = Slot::Late;
        None
    }

    /// The findings of earlier edits not yet shown, as one text block per
    /// edit (each labelled with its file).
    #[must_use]
    pub fn take_late(&self) -> Option<String> {
        let blocks = std::mem::take(&mut *lock(&self.inner.late));
        (!blocks.is_empty()).then(|| blocks.join("\n\n"))
    }

    /// Waits up to `lsp.wait_timeout_ms` for the running checks, then
    /// takes their findings (the turn is about to end).
    pub async fn settle(&self) -> Option<String> {
        let timeout = Duration::from_millis(self.inner.setup.config.wait_timeout_ms);
        let mut pending = self.inner.pending.subscribe();
        let _ = tokio::time::timeout(timeout, pending.wait_for(|n| *n == 0)).await;
        self.take_late()
    }

    /// Whether a check is running.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        *self.inner.pending.borrow() > 0
    }

    /// The edit chain `path` joins: a new one, or the running one (whose
    /// baseline the new edit is compared against).
    fn chain(&self, path: &Path) -> (u64, Arc<OnceCell<Vec<Diagnostic>>>) {
        let mut chains = lock(&self.inner.chains);
        let chain = chains.entry(path.to_path_buf()).or_insert_with(|| Chain {
            generation: 0,
            baseline: Arc::new(OnceCell::new()),
        });
        chain.generation += 1;
        (chain.generation, Arc::clone(&chain.baseline))
    }

    fn notice_once(&self, key: &str, line: &str) {
        self.inner.notice_once(key, line);
    }
}

/// One check.
struct Job {
    path: PathBuf,
    server: String,
    language: String,
    root: PathBuf,
    trusted: bool,
    before: Option<String>,
    after: String,
    generation: u64,
    baseline: Arc<OnceCell<Vec<Diagnostic>>>,
}

impl Inner {
    fn notice_once(&self, key: &str, line: &str) {
        if lock(&self.notified).insert(key.to_owned()) {
            tracing::info!(target: "pmt_tools::lsp", "{line}");
        }
    }

    /// Hands the verdict to the waiting write or queues it; a chain whose
    /// newer edit is checking reports nothing (the newer check will).
    fn deliver(&self, job: &Job, verdict: Option<Vec<String>>, slot: &Mutex<Slot>) {
        let newest = {
            let mut chains = lock(&self.chains);
            let newest = chains
                .get(&job.path)
                .is_some_and(|c| c.generation == job.generation);
            if newest {
                chains.remove(&job.path);
            }
            newest
        };
        let lines = verdict.filter(|_| newest).unwrap_or_default();
        let slot = lock(slot);
        match &*slot {
            Slot::Waiting(tx) => {
                let _ = tx.try_send(lines);
            }
            Slot::Late if !lines.is_empty() => lock(&self.late).push(format!(
                "LSP diagnostics introduced by your earlier edit of {}:\n{}",
                job.path.display(),
                lines.join("\n")
            )),
            Slot::Late => {}
        }
    }

    /// Runs one check; `None` when no verdict came.
    async fn check(&self, job: &Job) -> Option<Vec<String>> {
        let server = self.registry.get(&job.server)?.clone();
        let config = &self.setup.config;
        let warm = self.servers.is_running(&server.id, &job.root).await;
        if !warm && self.servers.is_broken(&server.id, &job.root) {
            return None;
        }
        let resolved = self.resolve(&server).await?;
        let pins = match self.pins(&server, &resolved, job.trusted).await {
            Ok(pins) => pins,
            Err(line) => {
                self.notice_once(&line, &line);
                return None;
            }
        };
        let timeout = Duration::from_millis(if warm {
            config.wait_timeout_ms
        } else {
            config.warmup_ms()
        });
        let deadline = tokio::time::Instant::now() + timeout;
        let launch = Launch {
            id: &server.id,
            root: &job.root,
            trusted_only: server.executes_project_code,
        };
        let running = match self
            .servers
            .get(launch, || {
                self.start(&server, &resolved, &job.root, pins, timeout)
            })
            .await
        {
            Ok(running) => running,
            Err(Unavailable::Broken) => return None,
            Err(Unavailable::Failed(why)) => {
                tracing::warn!(server = %server.id, "language server did not start: {why}");
                return None;
            }
        };
        let verdict = self.compare(job, &running, deadline).await;
        if verdict.is_err() {
            self.servers.mark_broken(&server.id, &job.root);
        }
        verdict.ok()
    }

    async fn compare(
        &self,
        job: &Job,
        running: &super::pool::Running,
        deadline: tokio::time::Instant,
    ) -> Result<Vec<String>, TimedOut> {
        let uri = protocol::uri(&job.path);
        let baseline = job
            .baseline
            .get_or_try_init(|| async {
                match &job.before {
                    None => Ok(Vec::new()),
                    Some(before) => {
                        check::diagnostics(running, &uri, &job.language, before, deadline).await
                    }
                }
            })
            .await?
            .clone();
        let full = self.setup.config.wait_mode == LspWaitMode::Full;
        let elsewhere_before = full.then(|| running.client.published());
        let after = check::diagnostics(running, &uri, &job.language, &job.after, deadline).await?;
        let mut lines: Vec<String> = protocol::introduced(&baseline, &after)
            .iter()
            .map(|d| d.render(&job.server))
            .collect();
        if let Some(before) = elsewhere_before {
            let now = check::workspace(running, deadline).await;
            lines.extend(check::introduced_elsewhere(
                &uri,
                &before,
                &now,
                &job.server,
            ));
        }
        Ok(lines)
    }

    /// The server's binary: found, or installed on first use when
    /// `lsp.install_strategy = "auto"` and lazy installs are allowed.
    async fn resolve(&self, server: &Server) -> Option<Resolved> {
        let strategy = self.setup.config.install_strategy;
        let installs = &self.setup.installs;
        if let Some(found) = installs.resolve(server, strategy) {
            return Some(found);
        }
        let installable = strategy == LspInstallStrategy::Auto
            && !server.command_overridden
            && !matches!(server.install, Install::Manual { .. });
        if !installable || !self.setup.toolchain.allow_lazy_install {
            let line = format!(
                "{} is not installed; `prometheus lsp install {}` or put it on PATH",
                server.id, server.id
            );
            self.notice_once(&format!("missing:{}", server.id), &line);
            return None;
        }
        let node = self
            .runtime(self.setup.toolchain.node.as_ref(), "node")
            .await;
        let python = self
            .runtime(self.setup.toolchain.python.as_ref(), "cpython")
            .await;
        let tools = Tools {
            manager: self.setup.config.package_manager,
            node: node.as_deref(),
            python: python.as_deref(),
        };
        match installs.install(server, &tools).await {
            Ok(_) => installs.resolve(server, strategy),
            Err(why) => {
                self.notice_once(&format!("install:{}", server.id), &format!("lsp: {why}"));
                None
            }
        }
    }

    async fn runtime(&self, policy: Option<&RuntimePolicy>, name: &str) -> Option<PathBuf> {
        policy?.executable(name).await.ok()
    }

    /// The settings and initialization options that keep a server which
    /// executes no project code that way in an untrusted workspace.
    async fn pins(
        &self,
        server: &Server,
        resolved: &Resolved,
        trusted: bool,
    ) -> Result<(Value, Map<String, Value>), String> {
        let mut options = Map::new();
        let mut settings = json!({});
        if !trusted {
            match server.pin {
                Pin::None => {}
                Pin::ManagedPython => {
                    let python = self
                        .runtime(self.setup.toolchain.python.as_ref(), "cpython")
                        .await
                        .ok_or_else(|| {
                            format!(
                                "skipped: {} needs the managed Python in an untrusted workspace",
                                server.id
                            )
                        })?;
                    settings = json!({"python": {"pythonPath": python, "venvPath": ""}});
                }
                Pin::BundledTypeScript => {
                    let tsserver = self
                        .setup
                        .installs
                        .dir(&server.id)
                        .join("node_modules/typescript/lib/tsserver.js");
                    if resolved.source != Source::Managed || !tsserver.is_file() {
                        return Err(format!(
                            "skipped: {} needs its bundled TypeScript in an untrusted workspace; run `prometheus lsp install {}`",
                            server.id, server.id
                        ));
                    }
                    options.insert("tsserver".into(), json!({"path": tsserver}));
                }
                Pin::SvelteUntrusted => {
                    options.insert("isTrusted".into(), Value::Bool(false));
                }
            }
        }
        options.extend(server.initialization_options.clone());
        Ok((settings, options))
    }

    /// The confined start of `server` for `root`.
    fn start<'a>(
        &self,
        server: &'a Server,
        resolved: &Resolved,
        root: &'a Path,
        (settings, options): (Value, Map<String, Value>),
        timeout: Duration,
    ) -> Result<Start<'a>, String> {
        let cache_dir = self
            .servers
            .status()
            .cache_root()
            .join(&server.id)
            .join(root_key(root));
        let mut reads = Vec::new();
        if resolved.source == Source::Managed {
            reads.push(self.setup.installs.dir(&server.id));
        }
        if matches!(server.install, Install::Npm { .. })
            && let Some(runtime) = self.node_runtime()
        {
            reads.push(runtime);
        }
        let spec = LaunchSpec {
            server,
            program: &resolved.program,
            args: &resolved.args,
            root,
            cache_dir: &cache_dir,
            reads,
            path_var: self.node_path(),
        };
        let command = self.setup.launcher.command(&spec)?;
        Ok(Start {
            id: &server.id,
            command,
            root,
            initialization_options: Value::Object(options),
            settings,
            timeout,
        })
    }

    /// The directory of the Node that runs npm servers (`#!/usr/bin/env
    /// node`): the managed one, else the one on `PATH`.
    fn node_runtime(&self) -> Option<PathBuf> {
        let node = self
            .setup
            .toolchain
            .node
            .as_ref()
            .and_then(|p| p.runtimes.installed("node"))
            .or_else(|| self.setup.installs.on_path("node"))?;
        let node = dunce::canonicalize(&node).unwrap_or(node);
        node.parent()?.parent().map(Path::to_path_buf)
    }

    /// `PATH` with the managed Node first, when it is installed (npm
    /// servers start through `#!/usr/bin/env node`).
    fn node_path(&self) -> Option<std::ffi::OsString> {
        let node = self
            .setup
            .toolchain
            .node
            .as_ref()?
            .runtimes
            .installed("node")?;
        super::install::node_path(Some(&node), self.setup.installs.path_var.as_deref())
    }
}

/// A short stable key of a workspace root (its cache directory name).
fn root_key(root: &Path) -> String {
    use sha2::Digest as _;
    use std::fmt::Write as _;
    let digest = sha2::Sha256::digest(root.to_string_lossy().as_bytes());
    digest.iter().take(6).fold(String::new(), |mut key, b| {
        let _ = write!(key, "{b:02x}");
        key
    })
}

impl crate::files::PostWrite for SessionDiagnostics {
    fn after_write(
        &self,
        path: &Path,
        before: Option<&str>,
        after: &str,
        started: std::time::Instant,
    ) -> Option<Vec<String>> {
        Self::after_write(self, path, before, after, started)
    }
}
