//! Task dumps (spec: observability, Runtime liveness watchdog and task
//! dump), written from any thread even while the async runtime is
//! blocked:
//!
//! - every OS thread of the process (`pmt_core::os_threads`) with its
//!   scheduler state, the tracked task it is polling right now and the
//!   span it has entered;
//! - every task the agent spawned (`pmt_core::spawn`, which every spawn
//!   site goes through) with where it was spawned, whether it is running
//!   (on which thread, for how long) and otherwise its await point: the
//!   innermost span it is suspended in;
//! - every open `tracing` span as a tree with its age: each is a unit of
//!   work in flight (a session, a turn, an RPC request, a scheduled run).
//!
//! [`TaskLayer`] keeps the span picture without locking on the hot path
//! (enter and exit are atomic stores) and forwards enters and exits to
//! the task tracker, which derives await points from them.

mod render;

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Instant;

use pmt_core::error::{Error, codes};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// The most field text one span keeps.
const FIELDS_MAX: usize = 300;

#[derive(Debug)]
struct SpanState {
    name: &'static str,
    target: &'static str,
    fields: Mutex<String>,
    parent: Option<u64>,
    opened: Instant,
    /// The OS id of the thread that has it entered (0: none).
    entered: AtomicU64,
}

static OPEN: LazyLock<Mutex<HashMap<u64, Arc<SpanState>>>> = LazyLock::new(Mutex::default);
/// Thread names by OS id, for OSes that offer no thread list.
static NAMES: LazyLock<Mutex<BTreeMap<u64, String>>> = LazyLock::new(Mutex::default);

thread_local! {
    /// The calling thread's OS id, its name recorded on first use.
    static THREAD: u64 = {
        let id = pmt_core::os_threads::current_id();
        let current = std::thread::current();
        let name = current.name().map_or_else(|| format!("{:?}", current.id()), str::to_owned);
        NAMES.lock().unwrap_or_else(PoisonError::into_inner).insert(id, name);
        id
    };
}

/// The `tracing` layer that keeps the open spans for dumps and feeds the
/// task tracker's await points.
#[derive(Debug, Clone, Copy, Default)]
pub struct TaskLayer;

impl<S> Layer<S> for TaskLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut fields = Fields(String::new());
        attrs.record(&mut fields);
        let parent = ctx
            .span(id)
            .and_then(|s| s.parent())
            .map(|p| p.id().into_u64());
        let state = Arc::new(SpanState {
            name: attrs.metadata().name(),
            target: attrs.metadata().target(),
            fields: Mutex::new(fields.0),
            parent,
            opened: Instant::now(),
            entered: AtomicU64::new(0),
        });
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(Arc::clone(&state));
        }
        OPEN.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.into_u64(), state);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if let Some(state) = state_of(&ctx, id) {
            let mut fields = state.fields.lock().unwrap_or_else(PoisonError::into_inner);
            let mut added = Fields(std::mem::take(&mut *fields));
            values.record(&mut added);
            *fields = added.0;
        }
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        pmt_core::spawn::entered(id.into_u64());
        if let Some(state) = state_of(&ctx, id) {
            let thread = THREAD
                .try_with(|t| *t)
                .unwrap_or_else(|_| pmt_core::os_threads::current_id());
            state.entered.store(thread, Ordering::Relaxed);
        }
    }

    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
        pmt_core::spawn::exited(id.into_u64());
        if let Some(state) = state_of(&ctx, id) {
            state.entered.store(0, Ordering::Relaxed);
        }
    }

    fn on_close(&self, id: Id, _ctx: Context<'_, S>) {
        OPEN.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id.into_u64());
    }

    fn register_callsite(&self, _: &'static Metadata<'static>) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::sometimes()
    }
}

fn state_of<S>(ctx: &Context<'_, S>, id: &Id) -> Option<Arc<SpanState>>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    ctx.span(id)?.extensions().get::<Arc<SpanState>>().cloned()
}

struct Fields(String);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if self.0.len() < FIELDS_MAX {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
}

/// The async runtime's counters, read without its cooperation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeCounters {
    /// Worker threads.
    pub workers: usize,
    /// Tasks alive.
    pub alive_tasks: usize,
    /// Tasks waiting in the global queue.
    pub global_queue: usize,
}

impl RuntimeCounters {
    /// The counters of `handle`'s runtime.
    #[must_use]
    pub fn of(handle: &tokio::runtime::Handle) -> Self {
        let metrics = handle.metrics();
        Self {
            workers: metrics.num_workers(),
            alive_tasks: metrics.num_alive_tasks(),
            global_queue: metrics.global_queue_depth(),
        }
    }
}

/// The dump text: why, the runtime counters, every thread, every tracked
/// task and every open span.
#[must_use]
pub fn render(reason: &str, runtime: Option<RuntimeCounters>) -> String {
    let open: Vec<(u64, Arc<SpanState>)> = OPEN
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|(id, s)| (*id, Arc::clone(s)))
        .collect();
    let names = NAMES.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let snapshot = render::Snapshot {
        open,
        tasks: pmt_core::spawn::tasks(),
        threads: pmt_core::os_threads::list(),
        names,
    };
    let mut out = format!(
        "prometheus task dump\nwhen: {}\npid: {}\nreason: {reason}\n",
        crate::trajectory::timestamp(jiff::Timestamp::now()),
        std::process::id()
    );
    snapshot.write(&mut out, runtime);
    out
}

/// Writes [`render`] to `<home>/logs/task-dump-<UTC timestamp>.txt`
/// (mode 0600); its path.
///
/// # Errors
///
/// `home.io` when the file cannot be written.
pub fn write(
    home: &Path,
    reason: &str,
    runtime: Option<RuntimeCounters>,
) -> Result<PathBuf, Error> {
    let stamp = jiff::Timestamp::now()
        .strftime("%Y%m%dT%H%M%S%.3fZ")
        .to_string();
    let dir = home.join("logs");
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot create", &dir, &e))?;
    let path = dir.join(format!("task-dump-{stamp}.txt"));
    pmt_core::private_fs::write_atomic(&path, render(reason, runtime).as_bytes())
        .map_err(|e| Error::io_doing(&codes::HOME_IO, "cannot write", &path, &e))?;
    Ok(path)
}

#[cfg(test)]
mod tests;
