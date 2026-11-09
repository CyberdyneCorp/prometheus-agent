//! A session's running servers, the profile's broken marks and idle
//! shutdown (spec: lsp-diagnostics, Wait modes and timeouts; LSP CLI).
//!
//! Servers are keyed by (server id, workspace root) and belong to the
//! session that started them: each runs under that session's sandbox and
//! egress policy, and stops when the session ends, when it has been idle
//! for `lsp.idle_timeout_secs`, or when `prometheus lsp restart` names it.
//! A server that fails to start or times out is marked broken for its
//! workspace in the whole process, and skipped until
//! `lsp.broken_retry_secs` pass (`0`: until a restart or process exit).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use super::client::{Client, Start};
use super::protocol::Diagnostic;
use super::status::{self, Entry, Status};

/// `(profile dir, server id, workspace root)`.
type BrokenKey = (PathBuf, String, PathBuf);

/// When each (server, workspace) pair of the process last broke.
static BROKEN: LazyLock<Mutex<HashMap<BrokenKey, Instant>>> = LazyLock::new(Mutex::default);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether a pair that broke at `since` is still skipped at `now`.
#[must_use]
pub fn still_broken(since: Instant, now: Instant, retry_secs: u64) -> bool {
    retry_secs == 0 || now.duration_since(since) < Duration::from_secs(retry_secs)
}

/// One open document of a server.
#[derive(Debug, Clone, Default)]
pub struct Document {
    /// The version last sent.
    pub version: i64,
    /// The text last sent.
    pub text: String,
    /// The diagnostics published for that version.
    pub diagnostics: Vec<Diagnostic>,
}

/// A started server.
pub struct Running {
    /// Its JSON-RPC session.
    pub client: Client,
    /// Its open documents; held across one check so checks of the server
    /// do not interleave their versions.
    pub documents: tokio::sync::Mutex<HashMap<String, Document>>,
    last_used: Mutex<Instant>,
    stopped: std::sync::atomic::AtomicBool,
    status: Status,
    id: String,
}

impl Running {
    /// Whether it still serves requests.
    #[must_use]
    pub fn usable(&self) -> bool {
        self.client.alive() && !self.stopped.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn touch(&self) {
        *lock(&self.last_used) = Instant::now();
    }

    async fn stop(&self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.client.shutdown().await;
        if let Some(pid) = self.client.pid() {
            self.status.remove(pid);
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(pid) = self.client.pid() {
            self.status.remove(pid);
        }
    }
}

/// Why no server serves a check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unavailable {
    /// Marked broken for this workspace.
    Broken,
    /// It could not start (the pair is now marked broken).
    Failed(String),
}

/// A session's servers.
pub struct Servers {
    profile_dir: PathBuf,
    session_id: String,
    status: Status,
    idle: Duration,
    retry_secs: u64,
    running: tokio::sync::Mutex<HashMap<(String, PathBuf), Arc<Running>>>,
    restarts_seen: Mutex<HashMap<String, u128>>,
    /// Restart marks older than this session are history.
    born: u128,
}

/// What identifies a server start.
pub struct Launch<'a> {
    /// The server id.
    pub id: &'a str,
    /// Its workspace root.
    pub root: &'a Path,
    /// Whether it executes project code (shown by `lsp status`).
    pub trusted_only: bool,
}

impl Servers {
    /// The servers of `session_id` in the profile at `profile_dir`.
    #[must_use]
    pub fn new(profile_dir: &Path, session_id: &str, idle_secs: u64, retry_secs: u64) -> Self {
        let status = Status::of_profile(profile_dir);
        Self {
            profile_dir: profile_dir.to_path_buf(),
            session_id: session_id.to_owned(),
            status,
            idle: Duration::from_secs(idle_secs),
            retry_secs,
            running: tokio::sync::Mutex::default(),
            restarts_seen: Mutex::default(),
            born: status::now_nanos(),
        }
    }

    /// The profile's status files.
    #[must_use]
    pub fn status(&self) -> &Status {
        &self.status
    }

    /// Whether a usable server of `id` runs for `root`.
    pub async fn is_running(&self, id: &str, root: &Path) -> bool {
        self.apply_restarts().await;
        self.running
            .lock()
            .await
            .get(&(id.to_owned(), root.to_path_buf()))
            .is_some_and(|r| r.usable())
    }

    /// Whether `id` is marked broken for `root`.
    #[must_use]
    pub fn is_broken(&self, id: &str, root: &Path) -> bool {
        let key = (self.profile_dir.clone(), id.to_owned(), root.to_path_buf());
        lock(&BROKEN)
            .get(&key)
            .is_some_and(|since| still_broken(*since, Instant::now(), self.retry_secs))
    }

    /// Marks `id` broken for `root` (it failed or timed out).
    pub fn mark_broken(&self, id: &str, root: &Path) {
        tracing::warn!(server = id, workspace = %root.display(), "language server marked broken");
        lock(&BROKEN).insert(
            (self.profile_dir.clone(), id.to_owned(), root.to_path_buf()),
            Instant::now(),
        );
    }

    /// The running server of `launch`, started if needed.
    ///
    /// # Errors
    ///
    /// It is marked broken, or failed to start (and is marked now).
    pub async fn get<'a>(
        &self,
        launch: Launch<'a>,
        start: impl FnOnce() -> Result<Start<'a>, String>,
    ) -> Result<Arc<Running>, Unavailable> {
        self.apply_restarts().await;
        let key = (launch.id.to_owned(), launch.root.to_path_buf());
        let mut running = self.running.lock().await;
        if let Some(server) = running.get(&key).filter(|r| r.usable()) {
            server.touch();
            return Ok(Arc::clone(server));
        }
        if self.is_broken(launch.id, launch.root) {
            return Err(Unavailable::Broken);
        }
        let started = match start() {
            Ok(start) => Client::start(start).await,
            Err(why) => Err(why),
        };
        let client = match started {
            Ok(client) => client,
            Err(why) => {
                self.mark_broken(launch.id, launch.root);
                return Err(Unavailable::Failed(why));
            }
        };
        let server = Arc::new(Running {
            client,
            documents: tokio::sync::Mutex::default(),
            last_used: Mutex::new(Instant::now()),
            stopped: std::sync::atomic::AtomicBool::new(false),
            status: self.status.clone(),
            id: launch.id.to_owned(),
        });
        if let Some(pid) = server.client.pid() {
            self.status.write(&Entry {
                id: launch.id.to_owned(),
                workspace: launch.root.display().to_string(),
                pid,
                started: pmt_core::runtime::process_start_time(pid)
                    .unwrap_or_else(status::now_secs),
                state: "ready".to_owned(),
                trusted_only: launch.trusted_only,
                session_id: self.session_id.clone(),
            });
        }
        self.watch_idle(&server);
        running.insert(key, Arc::clone(&server));
        Ok(server)
    }

    /// Stops a server idle for longer than `lsp.idle_timeout_secs`; the
    /// next check starts it again.
    fn watch_idle(&self, server: &Arc<Running>) {
        if self.idle.is_zero() {
            return;
        }
        let idle = self.idle;
        let weak: Weak<Running> = Arc::downgrade(server);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(idle / 4 + Duration::from_millis(10)).await;
                let Some(server) = weak.upgrade() else { return };
                if !server.usable() {
                    return;
                }
                if lock(&server.last_used).elapsed() >= idle {
                    tracing::debug!(server = %server.id, "language server idle: stopping");
                    server.stop().await;
                    return;
                }
            }
        });
    }

    /// Applies `prometheus lsp restart` marks set since this session last
    /// looked: stops the servers named and clears their broken marks.
    async fn apply_restarts(&self) {
        let mut running = self.running.lock().await;
        let mut ids: Vec<String> = running.keys().map(|(id, _)| id.clone()).collect();
        ids.extend(
            lock(&BROKEN)
                .keys()
                .filter(|(profile, _, _)| *profile == self.profile_dir)
                .map(|(_, id, _)| id.clone()),
        );
        ids.sort();
        ids.dedup();
        let all = self.changed(String::new(), self.status.restart_mark(None));
        let named: Vec<String> = ids
            .into_iter()
            .filter(|id| {
                let mark = self.status.restart_mark(Some(id));
                self.changed(id.clone(), mark) || all
            })
            .collect();
        if named.is_empty() {
            return;
        }
        lock(&BROKEN)
            .retain(|(profile, id, _), _| *profile != self.profile_dir || !named.contains(id));
        let stopping: Vec<Arc<Running>> = running
            .iter()
            .filter(|((id, _), _)| named.contains(id))
            .map(|(_, server)| Arc::clone(server))
            .collect();
        running.retain(|(id, _), _| !named.contains(id));
        drop(running);
        for server in stopping {
            server.stop().await;
        }
    }

    /// Whether `mark` of `key` (`""` = every server) is newer than the
    /// last one seen, or than the session.
    fn changed(&self, key: String, mark: u128) -> bool {
        let mut seen = lock(&self.restarts_seen);
        let previous = seen.get(&key).copied().unwrap_or(self.born);
        seen.insert(key, mark.max(previous));
        mark > previous
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_broken_pair_is_retried_only_after_the_retry_delay() {
        let broke = Instant::now();
        let two_minutes = broke + Duration::from_mins(2);
        let five_minutes = broke + Duration::from_mins(5);
        assert!(still_broken(broke, two_minutes, 300));
        assert!(!still_broken(broke, five_minutes, 300));
        // 0: until `lsp restart` or the process exits.
        assert!(still_broken(broke, broke + Duration::from_hours(24), 0));
    }
}
