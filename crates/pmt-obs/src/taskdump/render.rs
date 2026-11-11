//! The sections of a task dump: runtime counters, threads, tasks and the
//! open-span tree.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

use pmt_core::os_threads::OsThread;
use pmt_core::spawn::{Kind, State, TaskSnapshot};

use super::{RuntimeCounters, SpanState};

/// Everything a dump shows, captured once.
pub(super) struct Snapshot {
    pub(super) open: Vec<(u64, Arc<SpanState>)>,
    pub(super) tasks: Vec<TaskSnapshot>,
    /// `None`: the OS offers no thread list.
    pub(super) threads: Option<Vec<OsThread>>,
    /// Names of threads that entered a span, by OS id.
    pub(super) names: BTreeMap<u64, String>,
}

impl Snapshot {
    pub(super) fn write(&self, out: &mut String, runtime: Option<RuntimeCounters>) {
        self.runtime(out, runtime);
        self.threads(out);
        self.tasks(out);
        self.spans(out);
    }

    fn runtime(&self, out: &mut String, runtime: Option<RuntimeCounters>) {
        let tracked = self.tasks.iter().filter(|t| t.kind == Kind::Async).count();
        if let Some(r) = runtime {
            let _ = writeln!(
                out,
                "runtime: {} workers, {} tasks alive, {} queued globally",
                r.workers, r.alive_tasks, r.global_queue
            );
        }
        let _ = writeln!(
            out,
            "tracked: {tracked} async tasks and {} blocking jobs on every runtime \
             (other alive tasks were spawned inside dependencies)",
            self.tasks.len() - tracked
        );
    }

    fn threads(&self, out: &mut String) {
        let threads = self.threads.clone().unwrap_or_else(|| self.known_threads());
        match self.threads {
            Some(_) => {
                let _ = writeln!(out, "\nthreads ({}):", threads.len());
            }
            None => out.push_str(
                "\nthreads that ran tracked work or entered a span \
                 (this OS offers no thread list):\n",
            ),
        }
        for thread in &threads {
            self.thread(out, thread);
        }
    }

    /// Without an OS list: the threads seen running tasks or in spans.
    fn known_threads(&self) -> Vec<OsThread> {
        let mut ids: BTreeSet<u64> = self.names.keys().copied().collect();
        ids.extend(self.tasks.iter().filter_map(|t| match t.state {
            State::Running { thread, .. } => Some(thread),
            State::Idle { .. } => None,
        }));
        ids.into_iter()
            .map(|id| OsThread {
                id,
                name: None,
                state: None,
            })
            .collect()
    }

    fn thread(&self, out: &mut String, thread: &OsThread) {
        let name = thread
            .name
            .as_deref()
            .or_else(|| self.names.get(&thread.id).map(String::as_str))
            .unwrap_or("unnamed");
        let _ = write!(out, "  thread {} \"{name}\"", thread.id);
        if let Some(state) = &thread.state {
            let _ = write!(out, " ({state})");
        }
        out.push('\n');
        for task in &self.tasks {
            if let State::Running { thread: on, for_ } = task.state
                && on == thread.id
            {
                let _ = writeln!(
                    out,
                    "      running {} #{} for {:.1}s, spawned at {}",
                    kind(task.kind),
                    task.id,
                    for_.as_secs_f64(),
                    task.location
                );
            }
        }
        if let Some(span) = self.innermost_entered(thread.id) {
            let _ = writeln!(out, "      inside {}", chain(span, &self.open));
        }
    }

    /// The deepest span entered on thread `id`.
    fn innermost_entered(&self, id: u64) -> Option<u64> {
        self.open
            .iter()
            .filter(|(_, s)| s.entered.load(Ordering::Relaxed) == id)
            .max_by_key(|(span, _)| depth(*span, &self.open))
            .map(|(span, _)| *span)
    }

    fn tasks(&self, out: &mut String) {
        let mut tasks: Vec<&TaskSnapshot> = self.tasks.iter().collect();
        tasks.sort_by_key(|t| std::cmp::Reverse(t.age));
        let _ = writeln!(out, "\ntasks ({}), oldest first:", tasks.len());
        for task in tasks {
            let _ = write!(
                out,
                "  #{} {}, spawned at {}, age {:.1}s, {} polls: ",
                task.id,
                kind(task.kind),
                task.location,
                task.age.as_secs_f64(),
                task.polls
            );
            match task.state {
                State::Running { thread, for_ } => {
                    let _ = writeln!(
                        out,
                        "RUNNING on thread {thread} for {:.1}s",
                        for_.as_secs_f64()
                    );
                }
                State::Idle { for_ } => {
                    let _ = writeln!(
                        out,
                        "idle for {:.1}s, {}",
                        for_.as_secs_f64(),
                        self.await_point(task)
                    );
                }
            }
        }
    }

    fn await_point(&self, task: &TaskSnapshot) -> String {
        if task.polls == 0 {
            return "queued, not polled yet".to_owned();
        }
        match task.await_span {
            Some(span) if self.open.iter().any(|(id, _)| *id == span) => {
                format!("awaiting in {}", chain(span, &self.open))
            }
            Some(_) => "awaiting outside any open span (its last one closed)".to_owned(),
            None => "awaiting outside any span (it entered none)".to_owned(),
        }
    }

    fn spans(&self, out: &mut String) {
        let _ = writeln!(out, "\nopen spans ({}), oldest first:", self.open.len());
        let mut roots: Vec<&(u64, Arc<SpanState>)> = self
            .open
            .iter()
            .filter(|(_, s)| {
                s.parent
                    .is_none_or(|p| !self.open.iter().any(|(id, _)| *id == p))
            })
            .collect();
        roots.sort_by_key(|(_, s)| s.opened);
        for (id, _) in roots {
            tree(out, *id, &self.open, 1);
        }
    }
}

fn kind(kind: Kind) -> &'static str {
    match kind {
        Kind::Async => "async task",
        Kind::Blocking => "blocking job",
    }
}

/// How many open ancestors span `id` has.
fn depth(id: u64, open: &[(u64, Arc<SpanState>)]) -> usize {
    let mut depth = 0;
    let mut next = open
        .iter()
        .find(|(i, _)| *i == id)
        .and_then(|(_, s)| s.parent);
    while let Some(parent) = next {
        depth += 1;
        next = open
            .iter()
            .find(|(i, _)| *i == parent)
            .and_then(|(_, s)| s.parent);
    }
    depth
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
    let entered = match state.entered.load(Ordering::Relaxed) {
        0 => String::new(),
        thread => format!(" [entered on thread {thread}]"),
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
