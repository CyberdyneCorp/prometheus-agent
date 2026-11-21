//! The line interface's pet (spec: pets, Terminal pet rendering; One
//! activity-to-state mapping; Affection reactions): a pane right-aligned
//! above the status line, animated from the session's activity. It draws
//! only on a terminal, follows `/pet` and `prometheus pets` changes within
//! a moment and removes itself when the pet is turned off or the interface
//! ends.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use pmt_config::ProfileScope;
use serde_json::Value;

use super::runtime::Runtime;
use crate::chat::repl::bottom_line::PetFrame;
use crate::chat::repl::input::PetLine;

/// How often the pane advances a frame.
const TICK: Duration = Duration::from_millis(250);

fn frame(runtime: &Runtime, now: Instant) -> Option<PetFrame> {
    Some(PetFrame {
        sprite: runtime.sprite(now)?,
        heart: runtime.heart(now),
    })
}

/// The pet of one line interface.
pub struct LinePet {
    runtime: Arc<Mutex<Runtime>>,
    line: PetLine,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl LinePet {
    /// Starts the pane on `line` (animating on the current runtime).
    #[must_use]
    pub fn start(scope: &ProfileScope, env: &pmt_core::env::LaunchEnv, line: PetLine) -> Self {
        let runtime = Arc::new(Mutex::new(Runtime::new(scope, env)));
        let ticking = Arc::clone(&runtime);
        let drawing = line.clone();
        let task = tokio::runtime::Handle::try_current().ok().map(|handle| {
            pmt_core::spawn::spawn_on(&handle, async move {
                let mut shown: Option<PetFrame> = None;
                loop {
                    tokio::time::sleep(TICK).await;
                    let next = {
                        let mut runtime = ticking.lock().unwrap_or_else(PoisonError::into_inner);
                        let now = Instant::now();
                        runtime.tick(now);
                        frame(&runtime, now)
                    };
                    if next != shown {
                        drawing.show(next.clone());
                        shown = next;
                    }
                }
            })
        });
        Self {
            runtime,
            line,
            task,
        }
    }

    fn with(&self, f: impl FnOnce(&mut Runtime)) {
        f(&mut self.runtime.lock().unwrap_or_else(PoisonError::into_inner));
    }

    /// A trajectory event of the session.
    pub fn event(&self, kind: &str, payload: &Value) {
        self.with(|r| r.event(kind, payload));
    }

    /// The turn ended.
    pub fn turn_ended(&self, failed: bool) {
        self.with(|r| r.tracker.turn_ended(failed, Instant::now()));
    }

    /// Questions waiting for the user.
    pub fn waiting(&self, count: u32) {
        self.with(|r| r.tracker.set_waiting(count));
    }

    /// The user sent `text`: an affectionate one plays the heart (the text
    /// itself is left as it is).
    pub fn sent(&self, text: &str) {
        self.with(|r| r.sent(text));
    }

    /// An achievement unlocked: the pet jumps.
    pub fn celebrate(&self) {
        self.with(|r| r.tracker.celebrate(Instant::now()));
    }

    /// A handle that makes the pet jump from another task.
    #[must_use]
    pub fn celebrator(&self) -> Celebrator {
        Celebrator(Arc::clone(&self.runtime))
    }

    /// `/pet` changed the configuration: show it at once.
    pub fn refresh(&self) {
        let now = Instant::now();
        let next = {
            let mut runtime = self.runtime.lock().unwrap_or_else(PoisonError::into_inner);
            let delete = runtime.delete();
            runtime.refresh(true, now);
            if runtime.renderer().is_none() {
                erase(&delete);
            }
            frame(&runtime, now)
        };
        self.line.show(next);
    }
}

/// Makes a line interface's pet jump (an achievement unlocked).
#[derive(Clone)]
pub struct Celebrator(Arc<Mutex<Runtime>>);

impl Celebrator {
    /// The pet jumps.
    pub fn celebrate(&self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tracker
            .celebrate(Instant::now());
    }
}

/// Writes a graphics pet's delete sequence (nothing for text pets).
fn erase(delete: &str) {
    if !delete.is_empty() && crate::prompt::stdout_is_tty() {
        use std::io::Write as _;
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(delete.as_bytes());
        let _ = out.flush();
    }
}

impl Drop for LinePet {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let delete = self
            .runtime
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .delete();
        self.line.show(None);
        erase(&delete);
    }
}
