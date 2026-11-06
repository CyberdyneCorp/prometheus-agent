//! User-launched background tasks (`/background`, alias `/bg`; spec:
//! messaging-gateway, Background sessions from chat; subagent-delegation,
//! User-launched background sessions): what they tell the user, one
//! wording for chats, the line interface and the TUI, and the process's
//! [`Registry`] of them, numbered per launching session, that `/agents`
//! lists and stops.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Characters of the prompt the start notice repeats.
const EXCERPT: usize = 60;

/// How long a finished task stays listed.
pub const RETAIN: Duration = Duration::from_hours(1);

/// The source tag of a background task's session.
pub const SOURCE_TAG: &str = "background";

/// What starting task `number` answers (every surface says the same).
#[must_use]
pub fn started(number: u32, prompt: &str, id: &str) -> String {
    format!(
        "Background task #{number} started: {} (id {id})",
        excerpt(prompt)
    )
}

/// What task `number`, which answered `answer`, reports.
#[must_use]
pub fn complete(number: u32, answer: &str) -> String {
    format!("✅ Background task #{number} complete\n\n{answer}")
        .trim_end()
        .to_owned()
}

/// What task `number`, which failed for `reason`, reports.
#[must_use]
pub fn failed(number: u32, reason: &str) -> String {
    format!("❌ Background task #{number} failed: {reason}")
}

/// What refusing a task over the per-chat cap answers.
#[must_use]
pub fn too_many(max: u32) -> String {
    format!("Too many background tasks running ({max})")
}

/// What the start notice shows of `prompt`.
fn excerpt(prompt: &str) -> String {
    let flat = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= EXCERPT {
        return flat;
    }
    let cut: String = flat.chars().take(EXCERPT).collect();
    format!("{}…", cut.trim_end())
}

/// A task's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Waiting for a concurrency slot.
    Queued,
    /// Running.
    Running,
    /// Answered.
    Completed,
    /// Failed, with the reason.
    Failed(String),
    /// Stopped by the user.
    Interrupted,
}

impl State {
    /// The state's name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed(_) => "failed",
            Self::Interrupted => "interrupted",
        }
    }
}

/// Stops a task.
pub type Stop = Box<dyn Fn() + Send + Sync>;

/// One background task.
pub struct Task {
    /// `bg_<ULID>`.
    pub id: String,
    /// Its number in its launching session (from 1).
    pub number: u32,
    /// The launching session.
    pub launcher: String,
    /// The prompt.
    pub prompt: String,
    /// The `provider/model` it runs on.
    pub model: String,
    session: Mutex<Option<String>>,
    state: Mutex<State>,
    started: Instant,
    ended: Mutex<Option<Instant>>,
    stop: Stop,
}

impl std::fmt::Debug for Task {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Task")
            .field("id", &self.id)
            .field("number", &self.number)
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Task {
    /// Its session, once created.
    #[must_use]
    pub fn session(&self) -> Option<String> {
        lock(&self.session).clone()
    }

    /// Notes its session.
    pub fn set_session(&self, session: &str) {
        *lock(&self.session) = Some(session.to_owned());
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> State {
        lock(&self.state).clone()
    }

    /// It runs now.
    pub fn running(&self) {
        let mut state = lock(&self.state);
        if *state == State::Queued {
            *state = State::Running;
        }
    }

    /// It ended in `state` (a second end is ignored).
    pub fn finish(&self, state: State) {
        let mut ended = lock(&self.ended);
        if ended.is_none() {
            *ended = Some(Instant::now());
            *lock(&self.state) = state;
        }
    }

    /// Whether it still runs (queued or running).
    #[must_use]
    pub fn is_open(&self) -> bool {
        matches!(self.state(), State::Queued | State::Running)
    }

    /// Time since it started (until it ended).
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        lock(&self.ended)
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(self.started)
    }

    /// Asks it to stop.
    pub fn stop(&self) {
        (self.stop)();
    }

    fn expired(&self, now: Instant) -> bool {
        lock(&self.ended).is_some_and(|at| now.saturating_duration_since(at) >= RETAIN)
    }
}

/// What a new task is registered with.
pub struct NewTask {
    /// `bg_<ULID>`.
    pub id: String,
    /// The launching session.
    pub launcher: String,
    /// The prompt.
    pub prompt: String,
    /// The `provider/model` it runs on.
    pub model: String,
    /// Stops it.
    pub stop: Stop,
}

/// The background tasks of the process.
#[derive(Debug, Default)]
pub struct Registry {
    tasks: Mutex<Vec<Arc<Task>>>,
    numbers: Mutex<HashMap<String, u32>>,
}

static PROCESS: LazyLock<Registry> = LazyLock::new(Registry::default);

impl Registry {
    /// The process's registry.
    #[must_use]
    pub fn process() -> &'static Self {
        &PROCESS
    }

    /// Registers `new` (queued) under the next number of its launcher.
    pub fn add(&self, new: NewTask) -> Arc<Task> {
        let number = {
            let mut numbers = lock(&self.numbers);
            let n = numbers.entry(new.launcher.clone()).or_default();
            *n += 1;
            *n
        };
        let task = Arc::new(Task {
            id: new.id,
            number,
            launcher: new.launcher,
            prompt: new.prompt,
            model: new.model,
            session: Mutex::default(),
            state: Mutex::new(State::Queued),
            started: Instant::now(),
            ended: Mutex::default(),
            stop: new.stop,
        });
        let mut tasks = lock(&self.tasks);
        let now = Instant::now();
        tasks.retain(|t| !t.expired(now));
        tasks.push(Arc::clone(&task));
        task
    }

    /// The tasks `launcher` started (running and recently finished),
    /// oldest first.
    #[must_use]
    pub fn of(&self, launcher: &str) -> Vec<Arc<Task>> {
        let now = Instant::now();
        lock(&self.tasks)
            .iter()
            .filter(|t| t.launcher == launcher && !t.expired(now))
            .cloned()
            .collect()
    }

    /// `launcher`'s task `id` (its `bg_` id or `#<n>`).
    #[must_use]
    pub fn find(&self, launcher: &str, id: &str) -> Option<Arc<Task>> {
        let number = id.strip_prefix('#').and_then(|n| n.parse::<u32>().ok());
        self.of(launcher)
            .into_iter()
            .find(|t| t.id == id || Some(t.number) == number)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn a_long_prompt_is_cut_with_an_ellipsis() {
        let long = "word ".repeat(40);
        let cut = excerpt(&long);
        assert!(cut.ends_with('…') && cut.chars().count() <= EXCERPT + 1);
        assert_eq!(excerpt("  short\nprompt "), "short prompt");
        assert_eq!(too_many(3), "Too many background tasks running (3)");
        assert_eq!(
            started(2, "look", "bg_1"),
            "Background task #2 started: look (id bg_1)"
        );
    }

    #[test]
    fn tasks_are_numbered_per_launcher_and_stop_through_their_hook() {
        let registry = Registry::default();
        let stopped = Arc::new(AtomicBool::new(false));
        let hook = Arc::clone(&stopped);
        let new = |launcher: &str, stop: Stop| NewTask {
            id: format!("bg_{launcher}"),
            launcher: launcher.to_owned(),
            prompt: "p".to_owned(),
            model: "mock/mock".to_owned(),
            stop,
        };
        let first = registry.add(new("ses-a", Box::new(|| {})));
        let second = registry.add(new(
            "ses-a",
            Box::new(move || hook.store(true, Ordering::SeqCst)),
        ));
        let other = registry.add(new("ses-b", Box::new(|| {})));
        assert_eq!((first.number, second.number, other.number), (1, 2, 1));
        registry.find("ses-a", "#2").unwrap().stop();
        assert!(stopped.load(Ordering::SeqCst));
        assert!(registry.find("ses-b", "#2").is_none());
        second.finish(State::Interrupted);
        second.finish(State::Completed);
        assert_eq!(second.state(), State::Interrupted, "the first end holds");
    }
}
