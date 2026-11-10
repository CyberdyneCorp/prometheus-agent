//! Task dumps (spec: observability, Runtime liveness watchdog and task
//! dump): every open `tracing` span is a unit of work in flight (a
//! session, a turn, an RPC request, a scheduled run...), and the span a
//! thread has entered is where that thread is right now. [`TaskLayer`]
//! keeps that picture without locking on the hot path (enter and exit are
//! atomic stores), so a dump can be written from any thread even while
//! the async runtime is blocked: the threads that hold a span entered,
//! then every open span as a tree with its age, plus the runtime's own
//! counters when known.

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
    /// The key of the thread that has it entered (0: none).
    entered: AtomicU64,
}

static OPEN: LazyLock<Mutex<HashMap<u64, Arc<SpanState>>>> = LazyLock::new(Mutex::default);
static THREADS: LazyLock<Mutex<BTreeMap<u64, String>>> = LazyLock::new(Mutex::default);
static NEXT_THREAD: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static THREAD_KEY: u64 = {
        let key = NEXT_THREAD.fetch_add(1, Ordering::Relaxed);
        let current = std::thread::current();
        let name = current.name().map_or_else(|| format!("{:?}", current.id()), str::to_owned);
        THREADS.lock().unwrap_or_else(PoisonError::into_inner).insert(key, name);
        key
    };
}

/// The `tracing` layer that keeps the open spans for dumps.
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
        if let Some(state) = state_of(&ctx, id) {
            state
                .entered
                .store(THREAD_KEY.with(|k| *k), Ordering::Relaxed);
        }
    }

    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
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

/// The dump text: why, the runtime counters, the threads inside a span and
/// every open span as a tree (oldest first) with its age.
#[must_use]
pub fn render(reason: &str, runtime: Option<RuntimeCounters>) -> String {
    let open: Vec<(u64, Arc<SpanState>)> = OPEN
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|(id, s)| (*id, Arc::clone(s)))
        .collect();
    let threads = THREADS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let mut out = format!(
        "prometheus task dump\nwhen: {}\npid: {}\nreason: {reason}\n",
        crate::trajectory::timestamp(jiff::Timestamp::now()),
        std::process::id()
    );
    if let Some(r) = runtime {
        let _ = writeln!(
            out,
            "runtime: {} workers, {} tasks alive, {} queued globally",
            r.workers, r.alive_tasks, r.global_queue
        );
    }
    out.push_str("\nthreads inside a span (where each one is now):\n");
    let mut any = false;
    for (id, state) in &open {
        let key = state.entered.load(Ordering::Relaxed);
        if key != 0 {
            any = true;
            let thread = threads.get(&key).map_or("?", String::as_str);
            let _ = writeln!(out, "  thread {thread}: {}", chain(*id, &open));
        }
    }
    if !any {
        out.push_str("  (none)\n");
    }
    let _ = writeln!(out, "\nopen spans ({}), oldest first:", open.len());
    let mut roots: Vec<&(u64, Arc<SpanState>)> = open
        .iter()
        .filter(|(_, s)| {
            s.parent
                .is_none_or(|p| !open.iter().any(|(id, _)| *id == p))
        })
        .collect();
    roots.sort_by_key(|(_, s)| s.opened);
    for (id, _) in roots {
        tree(&mut out, *id, &open, 1);
    }
    out
}

/// `root > ... > span` of span `id`.
fn chain(id: u64, open: &[(u64, Arc<SpanState>)]) -> String {
    let mut names = Vec::new();
    let mut next = Some(id);
    while let Some(current) = next {
        let Some((_, state)) = open.iter().find(|(i, _)| *i == current) else {
            break;
        };
        names.push(describe(state));
        next = state.parent;
    }
    names.reverse();
    names.join(" > ")
}

fn describe(state: &SpanState) -> String {
    let fields = state.fields.lock().unwrap_or_else(PoisonError::into_inner);
    format!("{}::{}{}", state.target, state.name, fields)
}

fn tree(out: &mut String, id: u64, open: &[(u64, Arc<SpanState>)], depth: usize) {
    let Some((_, state)) = open.iter().find(|(i, _)| *i == id) else {
        return;
    };
    let age = state.opened.elapsed().as_secs_f64();
    let entered = if state.entered.load(Ordering::Relaxed) == 0 {
        ""
    } else {
        " [entered]"
    };
    let _ = writeln!(
        out,
        "{}{} ({age:.1}s){entered}",
        "  ".repeat(depth),
        describe(state)
    );
    let mut children: Vec<&(u64, Arc<SpanState>)> =
        open.iter().filter(|(_, s)| s.parent == Some(id)).collect();
    children.sort_by_key(|(_, s)| s.opened);
    for (child, _) in children {
        tree(out, *child, open, depth + 1);
    }
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
mod tests {
    use tracing_subscriber::prelude::*;

    use super::*;

    #[test]
    fn a_dump_shows_open_spans_and_the_thread_inside_one() {
        let subscriber = tracing_subscriber::registry().with(TaskLayer);
        tracing::subscriber::with_default(subscriber, || {
            let session = tracing::info_span!("session", session_id = "ses_1");
            let _in_session = session.enter();
            let turn = tracing::info_span!("turn", turn_id = "turn_1");
            let _in_turn = turn.enter();
            let text = render("test", None);
            assert!(
                text.contains("session session_id=\"ses_1\"")
                    || text.contains("::session session_id"),
                "{text}"
            );
            assert!(text.contains("turn turn_id=\"turn_1\" ("), "{text}");
            assert!(text.contains("> ") && text.contains("[entered]"), "{text}");
        });
    }
}
