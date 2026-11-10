//! The process's OS threads (spec: observability, Runtime liveness
//! watchdog and task dump): every thread of the process as the OS lists
//! it, with its name and scheduler state, and the OS id of the calling
//! thread, so a task dump can show every thread (the async runtime's
//! workers, blocking-pool threads, our own threads and any a dependency
//! started) and tie the work it knows about to the thread doing it.
//!
//! Linux and Android read `/proc/self/task`; macOS asks the Mach kernel
//! and Windows takes a Toolhelp snapshot (the only FFI here, in the
//! per-OS modules allowlisted for `unsafe` in health.toml). Elsewhere the
//! list is unavailable and thread ids are process-local numbers.

#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux;
#[cfg(any(target_os = "linux", target_os = "android"))]
use linux as os;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as os;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as os;

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    windows
)))]
mod os {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);

    pub(super) fn current_id() -> u64 {
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    pub(super) fn list() -> Option<Vec<super::OsThread>> {
        None
    }
}

/// One OS thread of this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsThread {
    /// The OS thread id (Linux tid, Mach thread id, Windows thread id).
    pub id: u64,
    /// Its name, when it has one.
    pub name: Option<String>,
    /// What the scheduler says it is doing (`running`, `sleeping`,
    /// `waiting in <kernel function>`...), when the OS tells.
    pub state: Option<String>,
}

thread_local! {
    static CURRENT: u64 = os::current_id();
}

/// The OS id of the calling thread (as [`OsThread::id`]); cached per
/// thread, so it is cheap on hot paths.
#[must_use]
pub fn current_id() -> u64 {
    CURRENT
        .try_with(|id| *id)
        .unwrap_or_else(|_| os::current_id())
}

/// Every thread of the process, by id; `None` where the OS offers no
/// list (or it could not be read).
#[must_use]
pub fn list() -> Option<Vec<OsThread>> {
    let mut threads = os::list()?;
    threads.sort_by_key(|t| t.id);
    Some(threads)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        windows
    ))]
    #[test]
    fn the_list_has_every_thread_with_its_name_and_the_callers_id() {
        let (ready, wait) = std::sync::mpsc::channel();
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::Builder::new()
            .name("pmt-listed".to_owned())
            .spawn(move || {
                ready.send(current_id()).unwrap();
                let _ = stopped.recv();
            })
            .unwrap();
        let worker_id = wait.recv().unwrap();
        let threads = list().expect("this OS lists threads");
        let _ = stop.send(());
        worker.join().unwrap();
        assert!(threads.iter().any(|t| t.id == current_id()), "{threads:?}");
        let listed = threads
            .iter()
            .find(|t| t.id == worker_id)
            .unwrap_or_else(|| panic!("worker {worker_id} missing: {threads:?}"));
        assert_eq!(listed.name.as_deref(), Some("pmt-listed"), "{threads:?}");
        assert_ne!(worker_id, current_id());
    }
}
