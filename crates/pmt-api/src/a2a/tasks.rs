//! A2A tasks in memory (spec: agent-interop, A2A JSON-RPC methods and task
//! lifecycle): each task moves `submitted` → `working` → `completed`,
//! `failed` or `canceled` (`rejected` without a turn), and every change
//! wakes the requests and streams waiting on it. A task waits for its
//! reply for `reply_timeout_secs`; nothing fails it earlier, and a
//! finished task is kept an hour for `GetTask`.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use jiff::Timestamp;
use pmt_a2a::wire::{self, Dialect, TaskState, TaskView, Turn};
use serde_json::{Value, json};
use tokio::sync::watch;

/// How long a finished task stays readable.
const KEEP_FINISHED: Duration = Duration::from_hours(1);
/// Tasks kept at most (the oldest finished ones go first).
const MAX_TASKS: usize = 10_000;

/// A push-notification configuration of a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PushConfig {
    pub id: String,
    pub url: String,
    pub token: Option<String>,
}

/// One task.
#[derive(Debug, Clone)]
pub(crate) struct Task {
    pub id: String,
    pub profile: String,
    /// The authenticated caller.
    pub identity: String,
    pub context_id: String,
    /// The conversation key it runs under.
    pub key: String,
    /// The dialect of the request that created it.
    pub dialect: Dialect,
    /// The session its turn runs in, once known.
    pub session: Option<String>,
    pub state: TaskState,
    pub status_text: Option<String>,
    pub reply: Option<String>,
    pub history: Vec<Turn>,
    pub updated: Timestamp,
    pub push: Vec<PushConfig>,
    /// Why the last push failed for good.
    pub push_error: Option<String>,
    /// The secret signing its pushes.
    pub push_secret: Option<String>,
}

impl Task {
    /// A new task in `submitted`.
    pub(crate) fn new(
        profile: &str,
        identity: &str,
        context_id: &str,
        key: &str,
        dialect: Dialect,
    ) -> Self {
        Self {
            id: ulid::Ulid::generate().to_string().to_lowercase(),
            profile: profile.to_owned(),
            identity: identity.to_owned(),
            context_id: context_id.to_owned(),
            key: key.to_owned(),
            dialect,
            session: None,
            state: TaskState::Submitted,
            status_text: None,
            reply: None,
            history: Vec::new(),
            updated: Timestamp::now(),
            push: Vec::new(),
            push_error: None,
            push_secret: None,
        }
    }

    /// The task object in `dialect`.
    pub(crate) fn json(&self, dialect: Dialect, history_length: Option<usize>) -> Value {
        let timestamp = self.updated.to_string();
        let mut out = wire::task(&self.view(&timestamp), dialect, history_length);
        if let Some(error) = &self.push_error {
            out["metadata"] = json!({"prometheus": {"push": {"state": "failed", "error": error}}});
        }
        out
    }

    /// A status-update event in `dialect`.
    pub(crate) fn status_event(&self, dialect: Dialect) -> Value {
        let timestamp = self.updated.to_string();
        wire::status_update(&self.view(&timestamp), dialect)
    }

    fn view<'a>(&'a self, timestamp: &'a str) -> TaskView<'a> {
        TaskView {
            id: &self.id,
            context_id: &self.context_id,
            state: self.state,
            status_text: self.status_text.as_deref(),
            reply: self.reply.as_deref(),
            history: &self.history,
            timestamp,
        }
    }
}

/// Every task of the listener.
pub(crate) struct Tasks {
    tasks: Mutex<HashMap<String, Task>>,
    changed: watch::Sender<u64>,
}

impl Default for Tasks {
    fn default() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            changed: watch::channel(0).0,
        }
    }
}

impl Tasks {
    /// Adds `task`, dropping finished tasks past their keep time.
    pub(crate) fn insert(&self, task: Task) {
        let mut tasks = self.tasks.lock().expect("tasks poisoned");
        let now = Timestamp::now();
        tasks.retain(|_, t| {
            !t.state.terminal() || now.duration_since(t.updated).unsigned_abs() < KEEP_FINISHED
        });
        if tasks.len() >= MAX_TASKS
            && let Some(oldest) = tasks
                .values()
                .filter(|t| t.state.terminal())
                .min_by_key(|t| t.updated)
                .map(|t| t.id.clone())
        {
            tasks.remove(&oldest);
        }
        tasks.insert(task.id.clone(), task);
        drop(tasks);
        self.changed.send_modify(|n| *n += 1);
    }

    /// Task `id` as `identity` of `profile` sees it (another caller's task
    /// does not exist for it).
    pub(crate) fn get(&self, profile: &str, identity: &str, id: &str) -> Option<Task> {
        let tasks = self.tasks.lock().expect("tasks poisoned");
        tasks
            .get(id)
            .filter(|t| t.profile == profile && t.identity == identity)
            .cloned()
    }

    /// Task `id`, whoever owns it.
    pub(crate) fn any(&self, id: &str) -> Option<Task> {
        self.tasks.lock().expect("tasks poisoned").get(id).cloned()
    }

    /// The tasks of `identity` of `profile`, newest first, in `context`
    /// and `state` when given.
    pub(crate) fn list(
        &self,
        profile: &str,
        identity: &str,
        context: Option<&str>,
        state: Option<TaskState>,
    ) -> Vec<Task> {
        let tasks = self.tasks.lock().expect("tasks poisoned");
        let mut out: Vec<Task> = tasks
            .values()
            .filter(|t| t.profile == profile && t.identity == identity)
            .filter(|t| context.is_none_or(|c| t.context_id == c))
            .filter(|t| state.is_none_or(|s| t.state == s))
            .cloned()
            .collect();
        out.sort_by(|a, b| b.updated.cmp(&a.updated).then(b.id.cmp(&a.id)));
        out
    }

    /// Applies `change` to task `id` (unless it already ended) and wakes
    /// its waiters; the task after the change.
    pub(crate) fn update(&self, id: &str, change: impl FnOnce(&mut Task)) -> Option<Task> {
        let mut tasks = self.tasks.lock().expect("tasks poisoned");
        let task = tasks.get_mut(id)?;
        change(task);
        task.updated = Timestamp::now();
        let out = task.clone();
        drop(tasks);
        self.changed.send_modify(|n| *n += 1);
        Some(out)
    }

    /// Moves task `id` to `state` unless it already ended; the task when
    /// it moved.
    pub(crate) fn transition(
        &self,
        id: &str,
        state: TaskState,
        reply: Option<String>,
        status_text: Option<String>,
    ) -> Option<Task> {
        let mut moved = false;
        let task = self.update(id, |task| {
            if !task.state.terminal() {
                task.state = state;
                if let Some(reply) = &reply {
                    task.history.push(Turn {
                        agent: true,
                        message_id: format!("{}-agent-{}", task.id, task.history.len()),
                        text: reply.clone(),
                    });
                }
                task.reply = reply;
                task.status_text = status_text;
                moved = true;
            }
        })?;
        moved.then_some(task)
    }

    /// Waits until task `id` ended or `deadline` passed; the task then.
    pub(crate) async fn wait_end(&self, id: &str, deadline: Duration) -> Option<Task> {
        let mut rx = self.changed.subscribe();
        let wait = async {
            loop {
                let task = self.any(id)?;
                if task.state.terminal() {
                    return Some(task);
                }
                if rx.changed().await.is_err() {
                    return Some(task);
                }
            }
        };
        match tokio::time::timeout(deadline, wait).await {
            Ok(task) => task,
            Err(_) => self.any(id),
        }
    }

    /// A receiver woken on every change.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }
}

/// How a turn ended: the reply, or why it failed.
pub(crate) type TurnResult = Result<String, String>;

/// Drives task `id` through its turn: `working` until `turn` answers
/// (`completed` with the reply as the artifact, `failed` with the reason)
/// or `timeout` passes (`failed`: no reply in time). `changed` is told
/// about every move. A task cancelled meanwhile stays cancelled.
pub(crate) async fn drive(
    tasks: &Tasks,
    id: &str,
    timeout: Duration,
    turn: impl Future<Output = TurnResult>,
    changed: &(dyn Fn(&Task) + Sync),
) -> Option<Task> {
    if let Some(task) = tasks.transition(id, TaskState::Working, None, None) {
        changed(&task);
    }
    let ended = match tokio::time::timeout(timeout, turn).await {
        Ok(Ok(reply)) => tasks.transition(id, TaskState::Completed, Some(reply), None),
        Ok(Err(why)) => tasks.transition(id, TaskState::Failed, None, Some(why)),
        Err(_) => tasks.transition(
            id,
            TaskState::Failed,
            None,
            Some(format!("no reply within {} s", timeout.as_secs())),
        ),
    };
    if let Some(task) = &ended {
        changed(task);
    }
    ended
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(tasks: &Tasks) -> String {
        let task = Task::new("default", "alice", "c1", "k", Dialect::V1);
        let id = task.id.clone();
        tasks.insert(task);
        id
    }

    /// Scenario: Long task without timeout loss.
    #[tokio::test(start_paused = true)]
    async fn a_long_turn_reports_working_until_its_reply() {
        let tasks = std::sync::Arc::new(Tasks::default());
        let id = task(&tasks);
        let driver = {
            let tasks = std::sync::Arc::clone(&tasks);
            let id = id.clone();
            pmt_core::spawn::spawn(async move {
                let turn = async {
                    tokio::time::sleep(Duration::from_secs(400)).await;
                    Ok("the reply".to_owned())
                };
                drive(&tasks, &id, Duration::from_mins(10), turn, &|_| {}).await
            })
        };
        tokio::time::sleep(Duration::from_secs(399)).await;
        assert_eq!(tasks.any(&id).unwrap().state, TaskState::Working);
        tokio::time::sleep(Duration::from_secs(2)).await;
        let done = driver.await.unwrap().unwrap();
        assert_eq!(done.state, TaskState::Completed);
        assert_eq!(done.reply.as_deref(), Some("the reply"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_turn_past_the_window_fails_only_then() {
        let tasks = Tasks::default();
        let id = task(&tasks);
        let turn = async {
            tokio::time::sleep(Duration::from_secs(400)).await;
            Ok("late".to_owned())
        };
        let started = tokio::time::Instant::now();
        let done = drive(&tasks, &id, Duration::from_mins(5), turn, &|_| {})
            .await
            .unwrap();
        assert_eq!(done.state, TaskState::Failed);
        assert_eq!(started.elapsed(), Duration::from_mins(5));
    }

    #[tokio::test]
    async fn a_cancelled_task_stays_cancelled() {
        let tasks = Tasks::default();
        let id = task(&tasks);
        tasks
            .transition(&id, TaskState::Canceled, None, None)
            .unwrap();
        let done = drive(
            &tasks,
            &id,
            Duration::from_secs(5),
            async { Ok("x".to_owned()) },
            &|_| {},
        )
        .await;
        assert!(done.is_none());
        assert_eq!(tasks.any(&id).unwrap().state, TaskState::Canceled);
        assert!(tasks.get("default", "bob", &id).is_none());
    }
}
