//! A local browser process a session owns (spec: execution-environments,
//! Browser supervision, dialogs and cleanup; Local browser options and
//! headed mode).
//!
//! Ending it kills the browser's process group, waits for it to exit (a
//! dying browser still writes to its profile) and removes its record, its
//! temp directory and its profile directory, unless the profile is kept
//! (`browser.user_data_dir` or a real-profile snapshot).

use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use pmt_net::egress::Proxy;
use tokio::process::Child;

use super::cleanup;
use super::launch::Launched;

/// How long a killed browser may take to exit before its files go anyway.
const EXIT_WAIT: Duration = Duration::from_secs(5);
/// How long a browser asked to close may take to write its profile and
/// exit (and, after that, its helpers). Generous because a loaded machine
/// slows the close down, and a kill before it finishes loses data; a
/// healthy browser ends the wait as soon as it exits.
const CLOSE_WAIT: Duration = Duration::from_secs(30);

/// A local browser process this session owns.
pub(super) struct LocalBrowser {
    child: Mutex<Option<Child>>,
    record: PathBuf,
    profile_dir: PathBuf,
    keep_profile: bool,
    temp_dir: PathBuf,
    pub(super) pid: u32,
    /// The egress proxy all its connections go through (stops on drop).
    _proxy: Proxy,
    /// Its OS sandbox (on Windows its `AppContainer`), kept while it runs.
    _sandbox: pmt_sandbox::Sandbox,
}

impl LocalBrowser {
    /// The browser of `launched`, recorded at `record`, behind `proxy`.
    pub(super) fn new(launched: Launched, record: PathBuf, proxy: Proxy) -> Self {
        Self {
            child: Mutex::new(Some(launched.child)),
            record,
            profile_dir: launched.profile_dir,
            keep_profile: launched.keep_profile,
            temp_dir: launched.temp_dir,
            pid: launched.pid,
            _proxy: proxy,
            _sandbox: launched.sandbox,
        }
    }

    fn take_child(&self) -> Option<Child> {
        self.child
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Whether the browser process still runs.
    pub(super) fn is_running(&self) -> bool {
        matches!(
            self.child
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_mut()
                .map(Child::try_wait),
            Some(Ok(None))
        )
    }

    /// The directories removed with the browser.
    fn owned_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = vec![self.temp_dir.clone()];
        if !self.keep_profile {
            dirs.push(self.profile_dir.clone());
        }
        dirs
    }

    /// Ends the browser: one whose profile is kept is asked to close over
    /// `cdp` and waited for, helpers included (a killed browser loses what
    /// it had not written yet, such as `localStorage`); any other, or one
    /// that does not close, is killed with its helpers and waited for.
    /// Then its files are removed.
    pub(super) async fn terminate(&self, cdp: &super::cdp::Cdp) {
        if let Some(child) = self.take_child() {
            self.end(child, cdp).await;
        }
        let _ = std::fs::remove_file(&self.record);
        for dir in self.owned_dirs() {
            cleanup::remove_dir_async(&dir).await;
        }
    }
}

impl LocalBrowser {
    /// Ends the process: a kept profile is closed gracefully when it can
    /// be (the wait is on the exit itself, not a fixed delay), anything
    /// else is killed with its group.
    async fn end(&self, mut child: Child, cdp: &super::cdp::Cdp) {
        if self.keep_profile && matches!(child.try_wait(), Ok(None)) {
            let _ = cdp
                .call_within("Browser.close", serde_json::json!({}), None, CLOSE_WAIT)
                .await;
            if tokio::time::timeout(CLOSE_WAIT, child.wait()).await.is_ok() {
                cleanup::drain_group(self.pid, CLOSE_WAIT).await;
                return;
            }
        }
        cleanup::kill_group(self.pid);
        let _ = child.start_kill();
        let _ = tokio::time::timeout(EXIT_WAIT, child.wait()).await;
    }
}

impl Drop for LocalBrowser {
    /// Best effort when a session is dropped without being closed: kill
    /// now, clean up once it exited.
    fn drop(&mut self) {
        let record = std::mem::take(&mut self.record);
        let dirs = self.owned_dirs();
        let remove = move || {
            let _ = std::fs::remove_file(&record);
            for dir in &dirs {
                cleanup::remove_dir(dir);
            }
        };
        let Some(mut child) = self.take_child() else {
            remove();
            return;
        };
        cleanup::kill_group(self.pid);
        let _ = child.start_kill();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    let _ = tokio::time::timeout(EXIT_WAIT, child.wait()).await;
                    let _ = tokio::task::spawn_blocking(remove).await;
                });
            }
            Err(_) => remove(),
        }
    }
}
