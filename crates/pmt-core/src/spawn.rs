//! Tracked spawning (spec: observability, Runtime liveness watchdog and
//! task dump): every async task and blocking job the agent starts goes
//! through this module (clippy's `disallowed-methods` refuses the tokio
//! spawn functions everywhere else), so a task dump can list each one with
//! where it was spawned, whether it is being polled right now (on which
//! OS thread, for how long: the stuck task of a stalled runtime) and,
//! between polls, its await point: the innermost `tracing` span it was
//! suspended in.
//!
//! The await point comes from span enter and exit events the dump layer
//! (`pmt_obs::taskdump::TaskLayer`) forwards to [`entered`] and
//! [`exited`]: the last span a poll exited right after entering a deeper
//! one is where the poll stopped descending, which is where the task
//! waits. Tasks a dependency spawns itself (connection tasks of HTTP
//! clients) cannot be tracked; a dump counts them from the runtime's own
//! alive-task number.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::future::Future;
use std::panic::Location;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::runtime::Handle;
use tokio::task::{AbortHandle, JoinHandle, JoinSet};

/// What kind of work a tracked task is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An async task.
    Async,
    /// A closure on the blocking pool.
    Blocking,
}

#[derive(Debug)]
struct Task {
    id: u64,
    kind: Kind,
    location: &'static Location<'static>,
    spawned: Instant,
    polls: AtomicU64,
    /// Nanoseconds after [`EPOCH`] the current poll began (0: not polled
    /// right now).
    polling_since: AtomicU64,
    /// Nanoseconds after [`EPOCH`] the last poll ended.
    idle_since: AtomicU64,
    /// The OS thread polling it (valid while `polling_since != 0`).
    thread: AtomicU64,
    /// The span id of its await point (0: none seen).
    await_span: AtomicU64,
}

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);
static TASKS: LazyLock<Mutex<BTreeMap<u64, Arc<Task>>>> = LazyLock::new(Mutex::default);
static NEXT: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// The task this thread is polling.
    static CURRENT: RefCell<Option<Arc<Task>>> = const { RefCell::new(None) };
    /// The last span event on this thread was an enter.
    static DESCENDING: Cell<bool> = const { Cell::new(false) };
}

fn now() -> u64 {
    // At least 1, so 0 can mean "not set".
    u64::try_from(EPOCH.elapsed().as_nanos())
        .unwrap_or(u64::MAX)
        .max(1)
}

fn since(nanos: u64) -> Duration {
    EPOCH.elapsed().saturating_sub(Duration::from_nanos(nanos))
}

impl Task {
    fn register(kind: Kind, location: &'static Location<'static>) -> Arc<Self> {
        let task = Arc::new(Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            kind,
            location,
            spawned: Instant::now(),
            polls: AtomicU64::new(0),
            polling_since: AtomicU64::new(0),
            idle_since: AtomicU64::new(now()),
            thread: AtomicU64::new(0),
            await_span: AtomicU64::new(0),
        });
        TASKS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(task.id, Arc::clone(&task));
        task
    }

    fn unregister(&self) {
        TASKS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}

/// Unregisters a blocking job when it ends.
struct Registered(Arc<Task>);

impl Drop for Registered {
    fn drop(&mut self) {
        self.0.unregister();
    }
}

/// Marks `task` as being polled on this thread until dropped.
struct Polling {
    task: Arc<Task>,
    previous: Option<Arc<Task>>,
}

impl Polling {
    fn begin(task: &Arc<Task>) -> Self {
        task.polls.fetch_add(1, Ordering::Relaxed);
        task.thread
            .store(crate::os_threads::current_id(), Ordering::Relaxed);
        task.polling_since.store(now(), Ordering::Relaxed);
        DESCENDING.with(|d| d.set(false));
        let previous = CURRENT.with(|c| c.replace(Some(Arc::clone(task))));
        Self {
            task: Arc::clone(task),
            previous,
        }
    }
}

impl Drop for Polling {
    fn drop(&mut self) {
        self.task.idle_since.store(now(), Ordering::Relaxed);
        self.task.polling_since.store(0, Ordering::Relaxed);
        let previous = self.previous.take();
        CURRENT.with(|c| *c.borrow_mut() = previous);
    }
}

/// A future registered as a tracked task until it completes or is
/// dropped.
#[derive(Debug)]
pub struct Tracked<F> {
    inner: Pin<Box<F>>,
    task: Arc<Task>,
}

impl<F: Future> Future for Tracked<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let _polling = Polling::begin(&self.task);
        self.inner.as_mut().poll(cx)
    }
}

impl<F> Drop for Tracked<F> {
    fn drop(&mut self) {
        self.task.unregister();
    }
}

/// `future`, registered as a tracked task spawned at the caller's
/// location (for spawners this module does not wrap).
#[track_caller]
pub fn track<F: Future>(future: F) -> Tracked<F> {
    Tracked {
        inner: Box::pin(future),
        task: Task::register(Kind::Async, Location::caller()),
    }
}

/// `tokio::spawn`, tracked.
#[track_caller]
#[allow(clippy::disallowed_methods)] // the one place it is called
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::task::spawn(track(future))
}

/// `Handle::spawn`, tracked.
#[track_caller]
#[allow(clippy::disallowed_methods)] // the one place it is called
pub fn spawn_on<F>(handle: &Handle, future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    handle.spawn(track(future))
}

/// `JoinSet::spawn`, tracked.
#[track_caller]
#[allow(clippy::disallowed_methods)] // the one place it is called
pub fn spawn_in<T, F>(set: &mut JoinSet<T>, future: F) -> AbortHandle
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    set.spawn(track(future))
}

/// `f`, registered as a blocking job while it runs.
#[track_caller]
fn blocking_job<F, R>(f: F) -> impl FnOnce() -> R + Send + 'static
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let task = Task::register(Kind::Blocking, Location::caller());
    move || {
        // Unregisters even when `f` panics.
        let _registered = Registered(Arc::clone(&task));
        let _polling = Polling::begin(&task);
        f()
    }
}

/// `tokio::task::spawn_blocking`, tracked.
#[track_caller]
#[allow(clippy::disallowed_methods)] // the one place it is called
pub fn blocking<F, R>(f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(blocking_job(f))
}

/// `Handle::spawn_blocking`, tracked.
#[track_caller]
#[allow(clippy::disallowed_methods)] // the one place it is called
pub fn blocking_on<F, R>(handle: &Handle, f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    handle.spawn_blocking(blocking_job(f))
}

/// Records that span `id` was entered on this thread (called by the dump
/// layer on every span enter).
pub fn entered(_id: u64) {
    // `try_with`: spans may close while thread-locals are torn down.
    let _ = DESCENDING.try_with(|d| d.set(true));
}

/// Records that span `id` was exited on this thread: right after an
/// enter, it is the deepest span of the poll so far, the polled task's
/// await point (called by the dump layer on every span exit).
pub fn exited(id: u64) {
    if DESCENDING.try_with(|d| d.replace(false)).unwrap_or(false) {
        let _ = CURRENT.try_with(|c| {
            if let Some(task) = c.borrow().as_ref() {
                task.await_span.store(id, Ordering::Relaxed);
            }
        });
    }
}

/// Where a tracked task is right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Being polled (or run, for a blocking job) on an OS thread.
    Running {
        /// The OS thread id ([`crate::os_threads::current_id`]).
        thread: u64,
        /// For how long.
        for_: Duration,
    },
    /// Suspended between polls.
    Idle {
        /// For how long.
        for_: Duration,
    },
}

/// One tracked task as a dump shows it.
#[derive(Debug, Clone)]
pub struct TaskSnapshot {
    /// A process-unique number.
    pub id: u64,
    /// Async task or blocking job.
    pub kind: Kind,
    /// Where it was spawned.
    pub location: &'static Location<'static>,
    /// Its age.
    pub age: Duration,
    /// Polls so far.
    pub polls: u64,
    /// Running or idle.
    pub state: State,
    /// The span id of its await point, when one was seen.
    pub await_span: Option<u64>,
}

/// Every tracked task alive, oldest first.
#[must_use]
pub fn tasks() -> Vec<TaskSnapshot> {
    let tasks: Vec<Arc<Task>> = TASKS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .values()
        .cloned()
        .collect();
    tasks
        .iter()
        .map(|t| {
            let polling = t.polling_since.load(Ordering::Relaxed);
            let state = if polling == 0 {
                State::Idle {
                    for_: since(t.idle_since.load(Ordering::Relaxed)),
                }
            } else {
                State::Running {
                    thread: t.thread.load(Ordering::Relaxed),
                    for_: since(polling),
                }
            };
            let span = t.await_span.load(Ordering::Relaxed);
            TaskSnapshot {
                id: t.id,
                kind: t.kind,
                location: t.location,
                age: t.spawned.elapsed(),
                polls: t.polls.load(Ordering::Relaxed),
                state,
                await_span: (span != 0).then_some(span),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(location_line: u32) -> Option<TaskSnapshot> {
        tasks()
            .into_iter()
            .find(|t| t.location.file() == file!() && t.location.line() == location_line)
    }

    #[tokio::test]
    async fn a_spawned_task_is_listed_where_it_was_spawned_until_it_ends() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let line = line!() + 1;
        let handle = spawn(async move {
            let _ = rx.await;
        });
        tokio::task::yield_now().await;
        let listed = find(line).expect("the task is listed");
        assert_eq!(listed.kind, Kind::Async);
        assert!(listed.polls >= 1, "{listed:?}");
        assert!(matches!(listed.state, State::Idle { .. }), "{listed:?}");
        tx.send(()).unwrap();
        handle.await.unwrap();
        assert!(find(line).is_none(), "a finished task is gone");
    }

    #[tokio::test]
    async fn a_running_blocking_job_names_its_thread() {
        let (started, wait) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let line = line!() + 1;
        let job = blocking(move || {
            started.send(crate::os_threads::current_id()).unwrap();
            let _ = released.recv();
        });
        let thread = wait.recv().unwrap();
        let listed = find(line).expect("the job is listed");
        assert_eq!(listed.kind, Kind::Blocking);
        assert!(
            matches!(listed.state, State::Running { thread: t, .. } if t == thread),
            "{listed:?}"
        );
        release.send(()).unwrap();
        job.await.unwrap();
        assert!(find(line).is_none());
    }

    #[test]
    fn the_await_point_is_the_deepest_span_of_the_last_descent() {
        let task = Task::register(Kind::Async, Location::caller());
        {
            let _polling = Polling::begin(&task);
            // outer(1) > sync scope(2), then outer > inner(3) where it waits.
            entered(1);
            entered(2);
            exited(2);
            entered(3);
            exited(3);
            exited(1);
        }
        assert_eq!(task.await_span.load(Ordering::Relaxed), 3);
        task.unregister();
    }
}
