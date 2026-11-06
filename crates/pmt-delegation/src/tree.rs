//! Live children and the spawn tree (spec: subagent-delegation, Control
//! actions scoped to the caller's spawn tree; Stop semantics).
//!
//! Every child is an [`Entry`] of the process-wide [`Registry`] from spawn
//! until an hour after it ended. A caller reaches only its own spawn tree:
//! children whose spawning conversation is the caller's (a conversation is
//! a session with its compression continuations), and, transitively,
//! their children. Any other id answers the same not-found message, so a
//! session cannot learn about another conversation's children.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use pmt_agent::turn::{Interrupt, Issuer};
use pmt_core::error::codes;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::Refusal;
use crate::progress::Snapshot;
use crate::result::SubagentResult;

/// How long an ended child stays reachable (status, wait, plugin result).
pub const RETAIN: Duration = Duration::from_hours(1);

/// A child's lifecycle state.
#[derive(Debug, Clone, PartialEq)]
pub enum State {
    /// Waiting for a concurrency slot.
    Queued,
    /// Running.
    Running,
    /// Ended, with its immutable result.
    Ended(Box<SubagentResult>),
}

impl State {
    /// The state's name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Ended(result) => result.status.as_str(),
        }
    }
}

/// Steer messages for a child: queued until its session can take them,
/// then handed to it as they arrive.
#[derive(Default)]
pub struct Steers {
    inner: Mutex<SteerState>,
}

type SteerTarget = Box<dyn Fn(&str) -> bool + Send + Sync>;

#[derive(Default)]
struct SteerState {
    pending: Vec<String>,
    target: Option<SteerTarget>,
    missed: Vec<String>,
}

impl Steers {
    fn lock(&self) -> MutexGuard<'_, SteerState> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `message` for the child's next iteration.
    pub fn push(&self, message: &str) {
        let mut state = self.lock();
        let delivered = state
            .target
            .as_ref()
            .is_some_and(|deliver| deliver(message));
        if !delivered {
            state.pending.push(message.to_owned());
        }
    }

    /// Binds the child's session: pending messages go to `deliver` now and
    /// later ones as they arrive.
    pub fn bind(&self, deliver: SteerTarget) {
        let mut state = self.lock();
        let pending = std::mem::take(&mut state.pending);
        for message in pending {
            if !deliver(&message) {
                state.missed.push(message);
            }
        }
        state.target = Some(deliver);
    }

    /// Ends delivery; `undelivered` are the messages the session never
    /// used. Returns every message that never landed.
    pub fn close(&self, undelivered: Vec<String>) -> Vec<String> {
        let mut state = self.lock();
        state.target = None;
        let mut missed = std::mem::take(&mut state.missed);
        missed.append(&mut state.pending);
        missed.extend(undelivered);
        missed
    }
}

impl std::fmt::Debug for Steers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Steers").finish_non_exhaustive()
    }
}

/// One child.
#[derive(Debug)]
pub struct Entry {
    /// `sa-<ULID>`.
    pub subagent_id: String,
    /// Its delegation.
    pub delegation_id: String,
    /// Its task's index.
    pub task_index: usize,
    /// The goal.
    pub goal: String,
    /// Depth from 1.
    pub depth: u32,
    /// The spawning session.
    pub parent_session: String,
    /// The spawning conversation (see the module docs).
    pub parent_conversation: String,
    /// The root lineage.
    pub root_session: String,
    /// It runs in the background (survives its spawning turn).
    pub background: bool,
    /// Stops it (a cooperative interrupt first).
    pub interrupt: Interrupt,
    /// Its steer messages.
    pub steers: Steers,
    session: Mutex<Option<String>>,
    model: Mutex<String>,
    snapshot: Mutex<Snapshot>,
    state: watch::Sender<State>,
    started: Instant,
    ended: Mutex<Option<Instant>>,
    cancel_requested: std::sync::atomic::AtomicBool,
    /// A plugin cancelled it (`cancel_requested`, then `cancelled`).
    cancelled: std::sync::atomic::AtomicBool,
}

/// What an entry is created from.
#[derive(Debug, Clone)]
pub struct NewEntry {
    /// `sa-<ULID>`.
    pub subagent_id: String,
    /// Its delegation.
    pub delegation_id: String,
    /// Its task's index.
    pub task_index: usize,
    /// The goal.
    pub goal: String,
    /// Depth from 1.
    pub depth: u32,
    /// The spawning session.
    pub parent_session: String,
    /// The spawning conversation.
    pub parent_conversation: String,
    /// The root lineage.
    pub root_session: String,
    /// Background child.
    pub background: bool,
    /// Queued at spawn.
    pub queued: bool,
}

impl Entry {
    /// The child's session, once built.
    #[must_use]
    pub fn session(&self) -> Option<String> {
        lock(&self.session).clone()
    }

    /// Notes the child's session and model; it runs now.
    pub fn started(&self, session: &str, model: &str) {
        *lock(&self.session) = Some(session.to_owned());
        model.clone_into(&mut lock(&self.model));
        self.state.send_if_modified(|s| {
            let queued = matches!(s, State::Queued);
            if queued {
                *s = State::Running;
            }
            queued
        });
    }

    /// Updates the live snapshot.
    pub fn observe(&self, update: impl FnOnce(&mut Snapshot)) {
        update(&mut lock(&self.snapshot));
    }

    /// The live snapshot.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        lock(&self.snapshot).clone()
    }

    /// Records the final result (a second call is ignored: results are
    /// immutable once terminal).
    pub fn finish(&self, result: &SubagentResult) {
        let changed = self.state.send_if_modified(|s| {
            let open = !matches!(s, State::Ended(_));
            if open {
                *s = State::Ended(Box::new(result.clone()));
            }
            open
        });
        if changed {
            *lock(&self.ended) = Some(Instant::now());
        }
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> State {
        self.state.borrow().clone()
    }

    /// Asks the child to stop (a cooperative, soft interrupt).
    pub fn request_stop(&self, issuer: Issuer) {
        self.cancel_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.interrupt.stop_soft(issuer);
    }

    /// A plugin's `cancel`: a stop the child's state reports as
    /// `cancel_requested`, then `cancelled`.
    pub fn request_cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.request_stop(Issuer::System("plugin_cancel".to_owned()));
    }

    /// Whether a plugin cancelled it.
    #[must_use]
    pub fn cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether a stop was requested.
    #[must_use]
    pub fn stop_requested(&self) -> bool {
        self.cancel_requested
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Waits until the child ends; returns its result.
    pub async fn wait(&self) -> SubagentResult {
        let mut rx = self.state.subscribe();
        loop {
            if let State::Ended(result) = &*rx.borrow_and_update() {
                return (**result).clone();
            }
            if rx.changed().await.is_err() {
                // The sender lives in `self`; this is unreachable while
                // `self` is borrowed, but stay total.
                return SubagentResult::ended(
                    &self.subagent_id,
                    self.task_index,
                    crate::result::Status::Interrupted,
                    crate::result::ExitReason::Interrupted,
                    None,
                );
            }
        }
    }

    /// The `list`/`status` view of the child.
    #[must_use]
    pub fn view(&self) -> Value {
        let snapshot = self.snapshot();
        let state = self.state();
        let mut view = json!({
            "subagent_id": self.subagent_id,
            "delegation_id": self.delegation_id,
            "task_index": self.task_index,
            "goal": self.goal,
            "depth": self.depth,
            "parent_session_id": self.parent_session,
            "status": state.name(),
            "elapsed_ms": u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "model": *lock(&self.model),
            "iteration": snapshot.iteration,
            "tool_calls": snapshot.tool_calls,
            "activity": snapshot.activity,
            "tokens": snapshot.usage.tokens,
            "cost_usd": snapshot.usage.cost_usd,
        });
        if let State::Ended(result) = state {
            view["result"] = serde_json::to_value(&*result).unwrap_or(Value::Null);
        }
        view
    }

    fn expired(&self, now: Instant) -> bool {
        lock(&self.ended).is_some_and(|at| now.saturating_duration_since(at) >= RETAIN)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Every child of the process.
#[derive(Debug, Default)]
pub struct Registry {
    entries: Mutex<HashMap<String, Arc<Entry>>>,
}

static PROCESS: LazyLock<Arc<Registry>> = LazyLock::new(Arc::default);

/// The not-in-tree answer for `id`.
#[must_use]
pub fn not_in_tree(id: &str) -> Refusal {
    Refusal::new(
        &codes::DELEGATION_NOT_IN_TREE,
        format!("No live subagent '{id}' in this conversation's spawn tree."),
    )
}

impl Registry {
    /// The process's registry.
    #[must_use]
    pub fn process() -> Arc<Self> {
        Arc::clone(&PROCESS)
    }

    /// Adds a child whose interrupt is a child of `parent_token`.
    pub fn add(&self, new: NewEntry, interrupt: Interrupt) -> Arc<Entry> {
        let initial = if new.queued {
            State::Queued
        } else {
            State::Running
        };
        let entry = Arc::new(Entry {
            subagent_id: new.subagent_id,
            delegation_id: new.delegation_id,
            task_index: new.task_index,
            goal: new.goal,
            depth: new.depth,
            parent_session: new.parent_session,
            parent_conversation: new.parent_conversation,
            root_session: new.root_session,
            background: new.background,
            interrupt,
            steers: Steers::default(),
            session: Mutex::default(),
            model: Mutex::default(),
            snapshot: Mutex::default(),
            state: watch::Sender::new(initial),
            started: Instant::now(),
            ended: Mutex::default(),
            cancel_requested: std::sync::atomic::AtomicBool::new(false),
            cancelled: std::sync::atomic::AtomicBool::new(false),
        });
        let mut entries = lock(&self.entries);
        let now = Instant::now();
        entries.retain(|_, e| !e.expired(now));
        entries.insert(entry.subagent_id.clone(), Arc::clone(&entry));
        entry
    }

    /// Child `id`, if it is in `conversation`'s spawn tree.
    ///
    /// # Errors
    ///
    /// The not-in-tree refusal.
    pub fn in_tree(&self, conversation: &str, id: &str) -> Result<Arc<Entry>, Refusal> {
        self.tree(conversation)
            .into_iter()
            .find(|e| e.subagent_id == id)
            .ok_or_else(|| not_in_tree(id))
    }

    /// `conversation`'s spawn tree, oldest first.
    #[must_use]
    pub fn tree(&self, conversation: &str) -> Vec<Arc<Entry>> {
        let entries: Vec<Arc<Entry>> = lock(&self.entries).values().cloned().collect();
        let mut owners = vec![conversation.to_owned()];
        let mut tree: Vec<Arc<Entry>> = Vec::new();
        loop {
            let found: Vec<Arc<Entry>> = entries
                .iter()
                .filter(|e| owners.contains(&e.parent_conversation))
                .filter(|e| !tree.iter().any(|t| t.subagent_id == e.subagent_id))
                .cloned()
                .collect();
            if found.is_empty() {
                break;
            }
            owners.extend(found.iter().filter_map(|e| e.session()));
            tree.extend(found);
        }
        tree.sort_by_key(|e| e.started);
        tree
    }

    /// Every child spawned by `session` itself that is still open.
    #[must_use]
    pub fn open_children_of(&self, conversation: &str) -> Vec<Arc<Entry>> {
        self.tree(conversation)
            .into_iter()
            .filter(|e| !matches!(e.state(), State::Ended(_)))
            .collect()
    }

    /// Child `id` wherever it is (the plugin interface checks ownership
    /// through its handle instead).
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<Entry>> {
        let entries = lock(&self.entries);
        entries
            .get(id)
            .filter(|e| !e.expired(Instant::now()))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, parent_conversation: &str) -> NewEntry {
        NewEntry {
            subagent_id: id.to_owned(),
            delegation_id: "dg-1".to_owned(),
            task_index: 0,
            goal: "g".to_owned(),
            depth: 1,
            parent_session: parent_conversation.to_owned(),
            parent_conversation: parent_conversation.to_owned(),
            root_session: "root".to_owned(),
            background: true,
            queued: false,
        }
    }

    /// Scenario: Foreign child cannot be steered.
    #[test]
    fn another_conversations_child_is_not_in_the_tree() {
        let registry = Registry::default();
        registry.add(entry("sa-mine", "ses-a"), Interrupt::default());
        registry.add(entry("sa-theirs", "ses-b"), Interrupt::default());
        assert!(registry.in_tree("ses-a", "sa-mine").is_ok());
        let err = registry.in_tree("ses-a", "sa-theirs").unwrap_err();
        assert_eq!(
            err.message,
            "No live subagent 'sa-theirs' in this conversation's spawn tree."
        );
    }

    #[test]
    fn grandchildren_are_in_the_tree_and_siblings_are_not() {
        let registry = Registry::default();
        let child = registry.add(entry("sa-child", "ses-root"), Interrupt::default());
        child.started("ses-child", "mock/mock");
        registry.add(entry("sa-grandchild", "ses-child"), Interrupt::default());
        registry.add(entry("sa-sibling", "ses-root"), Interrupt::default());
        let ids: Vec<String> = registry
            .tree("ses-root")
            .iter()
            .map(|e| e.subagent_id.clone())
            .collect();
        assert_eq!(ids.len(), 3);
        // The child sees only its own child.
        let ids: Vec<String> = registry
            .tree("ses-child")
            .iter()
            .map(|e| e.subagent_id.clone())
            .collect();
        assert_eq!(ids, ["sa-grandchild"]);
    }

    #[test]
    fn steers_wait_for_the_session_and_unused_ones_are_missed() {
        let steers = Steers::default();
        steers.push("focus on tests");
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&got);
        steers.bind(Box::new(move |m| {
            sink.lock().unwrap().push(m.to_owned());
            true
        }));
        steers.push("and docs");
        assert_eq!(*got.lock().unwrap(), ["focus on tests", "and docs"]);
        assert_eq!(steers.close(vec!["and docs".to_owned()]), ["and docs"]);
    }

    #[tokio::test]
    async fn results_are_immutable_once_terminal() {
        use crate::result::{ExitReason, Status};
        let registry = Registry::default();
        let e = registry.add(entry("sa-x", "ses"), Interrupt::default());
        e.finish(&SubagentResult::ended(
            "sa-x",
            0,
            Status::Completed,
            ExitReason::FinalAnswer,
            None,
        ));
        e.finish(&SubagentResult::ended(
            "sa-x",
            0,
            Status::Failed,
            ExitReason::Error,
            None,
        ));
        assert_eq!(e.wait().await.status, Status::Completed);
    }
}
