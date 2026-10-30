//! The pre-execution guard chain (spec: execution-environments,
//! Pre-execution guards fail closed).
//!
//! Every guard (capability check, approval gate, workdir validation) must
//! pass, in order, before anything runs, and the whole chain is bounded by
//! `max(timeout, 30)` seconds. A denial, an error or a missed deadline all
//! mean the command is not run.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// The shortest deadline a guard chain gets.
pub const MIN_DEADLINE_SECS: u64 = 30;

/// What a guard sees about the command about to run.
#[derive(Debug, Clone)]
pub struct GuardRequest {
    /// The command line.
    pub command: String,
    /// The directory it would run in.
    pub cwd: PathBuf,
    /// Whether it would run in the background.
    pub background: bool,
}

/// Why a guard stopped a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// The guard decided the command must not run; the message says why
    /// (for example an approval denial).
    Denied(String),
    /// The guard itself failed; the command is not run either.
    Failed(String),
}

/// A future returned by a [`Guard`].
pub type GuardFuture<'a> = Pin<Box<dyn Future<Output = Result<(), Stop>> + Send + 'a>>;

/// One step of the chain.
pub trait Guard: Send + Sync {
    /// A short name for messages (`approval`, `workdir`...).
    fn name(&self) -> &str;
    /// Resolves to `Ok` when the command may run.
    fn check<'a>(&'a self, request: &'a GuardRequest) -> GuardFuture<'a>;
}

/// An ordered list of guards with a deadline.
#[derive(Clone, Default)]
pub struct GuardChain {
    guards: Vec<Arc<dyn Guard>>,
}

impl std::fmt::Debug for GuardChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.guards.iter().map(|g| g.name()).collect();
        f.debug_struct("GuardChain")
            .field("guards", &names)
            .finish()
    }
}

impl GuardChain {
    /// A chain of `guards`, run in order.
    #[must_use]
    pub fn new(guards: Vec<Arc<dyn Guard>>) -> Self {
        Self { guards }
    }

    /// Appends a guard.
    pub fn push(&mut self, guard: Arc<dyn Guard>) {
        self.guards.push(guard);
    }

    /// The guards, in order.
    #[must_use]
    pub fn guards(&self) -> &[Arc<dyn Guard>] {
        &self.guards
    }

    /// Runs every guard under a deadline of `max(timeout_secs, 30)`.
    ///
    /// # Errors
    ///
    /// The message the `status = "error"` result carries:
    /// `pre-execution guard did not finish within <n>s; the command was not
    /// run` when the deadline passes, the guard's own message on a denial,
    /// `pre-execution guard '<name>' failed: <why>; the command was not run`
    /// on a guard error.
    pub async fn run(&self, request: &GuardRequest, timeout_secs: u64) -> Result<(), String> {
        let deadline = timeout_secs.max(MIN_DEADLINE_SECS);
        let chain = async {
            for guard in &self.guards {
                guard.check(request).await.map_err(|stop| match stop {
                    Stop::Denied(why) => why,
                    Stop::Failed(why) => format!(
                        "pre-execution guard '{}' failed: {why}; the command was not run",
                        guard.name()
                    ),
                })?;
            }
            Ok(())
        };
        tokio::time::timeout(Duration::from_secs(deadline), chain)
            .await
            .unwrap_or_else(|_| {
                Err(format!(
                    "pre-execution guard did not finish within {deadline}s; the command was not run"
                ))
            })
    }
}

/// Refuses a working directory that does not exist or is not a directory.
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkdirGuard;

impl Guard for WorkdirGuard {
    fn name(&self) -> &'static str {
        "workdir"
    }

    fn check<'a>(&'a self, request: &'a GuardRequest) -> GuardFuture<'a> {
        Box::pin(async move { check_workdir(&request.cwd) })
    }
}

fn check_workdir(cwd: &Path) -> Result<(), Stop> {
    match std::fs::metadata(cwd) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(Stop::Denied(format!(
            "workdir is not a directory: {}",
            cwd.display()
        ))),
        Err(_) => Err(Stop::Denied(format!(
            "workdir does not exist: {}",
            cwd.display()
        ))),
    }
}

/// A guard from a closure, for capability checks and tests.
pub struct FnGuard<F> {
    name: &'static str,
    check: F,
}

impl<F> FnGuard<F>
where
    F: Fn(&GuardRequest) -> Result<(), Stop> + Send + Sync,
{
    /// A guard named `name` that runs `check`.
    pub fn new(name: &'static str, check: F) -> Self {
        Self { name, check }
    }
}

impl<F> Guard for FnGuard<F>
where
    F: Fn(&GuardRequest) -> Result<(), Stop> + Send + Sync,
{
    fn name(&self) -> &str {
        self.name
    }

    fn check<'a>(&'a self, request: &'a GuardRequest) -> GuardFuture<'a> {
        let result = (self.check)(request);
        Box::pin(async move { result })
    }
}
