//! Watches a child's own trajectory events: its live snapshot (iteration,
//! tool, activity, usage), the stall monitor's activity, the files it
//! writes, and the paced `subagent.progress` events on the root session's
//! stream (spec: subagent-delegation, Live child progress streaming;
//! Structured result contract, artifacts).

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use pmt_agent::turn::EventSink;
use pmt_agent::turn::model::BoxFuture;
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::writer::Emit;
use pmt_store::Store;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::sync::watch;

use crate::progress::{self, Identity, Snapshot};
use crate::result::Artifact;
use crate::stall::Activity;
use crate::tree::Entry;

/// Largest artifact kept as a blob.
pub(super) const MAX_BLOB_BYTES: u64 = 1024 * 1024;

/// Tools whose `path` argument names a file they write.
const WRITERS: &[&str] = &["write_file", "patch"];

/// A child's event observer (cheap to clone).
#[derive(Clone)]
pub struct Tracker {
    inner: Arc<Inner>,
}

struct Inner {
    entry: Arc<Entry>,
    activity: Arc<Activity>,
    updates: watch::Sender<Snapshot>,
    written: Mutex<Vec<String>>,
    cwd: Mutex<Option<PathBuf>>,
}

impl Tracker {
    /// Starts tracking `entry`; with `root_events`, its progress is paced
    /// onto that stream.
    #[must_use]
    pub fn start(
        entry: Arc<Entry>,
        activity: Arc<Activity>,
        root_events: Option<Arc<dyn EventSink>>,
    ) -> Self {
        let (updates, rx) = watch::channel(Snapshot::default());
        if let Some(events) = root_events {
            let identity = Identity {
                subagent_id: entry.subagent_id.clone(),
                parent_id: entry.parent_session.clone(),
                depth: entry.depth,
            };
            let runtime = tokio::runtime::Handle::current();
            tokio::spawn(progress::pace(rx, move |snapshot| {
                let event = Event::SubagentProgress(identity.event(&snapshot));
                let events = Arc::clone(&events);
                runtime.spawn(async move {
                    if let Err(err) = events.emit(Emit::now(event)).await {
                        tracing::debug!(error = %err, "subagent.progress not recorded");
                    }
                });
            }));
        }
        Self {
            inner: Arc::new(Inner {
                entry,
                activity,
                updates,
                written: Mutex::default(),
                cwd: Mutex::default(),
            }),
        }
    }

    /// Resolves relative artifact paths against `cwd` (the child's
    /// working directory).
    pub fn set_cwd(&self, cwd: PathBuf) {
        *self
            .inner
            .cwd
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(cwd);
    }

    fn observe(&self, event: &Event) {
        let inner = &self.inner;
        match event {
            Event::ModelCall(call) => {
                inner.activity.progressed();
                inner.entry.observe(|s| {
                    s.iteration += 1;
                    let t = &mut s.usage.tokens;
                    t.input += call.tokens.input;
                    t.output += call.tokens.output;
                    t.cache_read += call.tokens.cache_read;
                    t.cache_write += call.tokens.cache_write;
                    t.reasoning += call.tokens.reasoning;
                    s.usage.cost_usd = match (s.usage.cost_usd, call.cost_usd) {
                        (Some(a), Some(b)) => Some(a + b),
                        (None, Some(b)) if s.iteration == 1 => Some(b),
                        _ => None,
                    };
                    s.activity = format!("model call {}", s.iteration);
                });
            }
            Event::ToolCall(call) => {
                inner.activity.tool_started();
                if WRITERS.contains(&call.tool.as_str())
                    && let Some(path) = call.arguments.get("path").and_then(Value::as_str)
                {
                    let mut written = inner.written.lock().unwrap_or_else(PoisonError::into_inner);
                    if !written.iter().any(|p| p == path) {
                        written.push(path.to_owned());
                    }
                }
                inner.entry.observe(|s| {
                    s.tool_calls += 1;
                    s.tool = Some(call.tool.clone());
                    s.activity = format!("{} {}", call.tool, argument_hint(&call.arguments));
                });
            }
            Event::ToolResult(_) => {
                inner.activity.progressed();
                inner.entry.observe(|s| s.tool = None);
            }
            _ => return,
        }
        inner.updates.send_replace(inner.entry.snapshot());
    }

    /// The files the child wrote, with their digests now; files of at most
    /// [`MAX_BLOB_BYTES`] are kept in `store`'s blobs.
    #[must_use]
    pub fn artifacts(&self, store: &Store) -> Vec<Artifact> {
        let written = self
            .inner
            .written
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let cwd = self
            .inner
            .cwd
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        written
            .into_iter()
            .filter_map(|path| artifact(store, cwd.as_deref(), &self.inner.entry.subagent_id, path))
            .collect()
    }
}

fn artifact(
    store: &Store,
    cwd: Option<&std::path::Path>,
    owner: &str,
    path: String,
) -> Option<Artifact> {
    let full = match cwd {
        Some(cwd) => cwd.join(&path),
        None => PathBuf::from(&path),
    };
    let bytes = std::fs::read(&full).ok()?;
    let sha256 = hex(&Sha256::digest(&bytes));
    let small = u64::try_from(bytes.len()).is_ok_and(|n| n <= MAX_BLOB_BYTES);
    let blob = small
        .then(|| store.put_blob(&bytes).ok())
        .flatten()
        .and_then(|blob| {
            store
                .reference_blob(&blob, "subagent", owner)
                .ok()
                .map(|()| format!("blobs/{}", blob.sha256()))
        });
    Some(Artifact { path, sha256, blob })
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// A short hint of what a call works on (its path, command or query).
fn argument_hint(arguments: &Value) -> String {
    ["path", "command", "query", "url", "pattern"]
        .iter()
        .find_map(|key| arguments.get(key).and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned()
}

impl EventSink for Tracker {
    fn emit(&self, emit: Emit) -> BoxFuture<'_, Result<(), pmt_core::Error>> {
        self.observe(&emit.event);
        Box::pin(async { Ok(()) })
    }
}
