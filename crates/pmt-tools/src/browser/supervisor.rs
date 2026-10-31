//! The CDP supervisor of one browser session (spec: execution-environments,
//! Browser supervision, dialogs and cleanup).
//!
//! It watches the DevTools connection: JavaScript dialogs are answered by
//! `browser.dialog_policy` (`must_respond` leaves them to the model and
//! dismisses them after `browser.dialog_timeout_secs`), and a dropped
//! connection is re-established, giving up after 5 consecutive failures.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use pmt_config::schema::DialogPolicy;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use super::cdp::{Cdp, Event};

/// Consecutive reconnect failures before the supervisor gives up.
pub const MAX_RECONNECTS: u32 = 5;
/// Wait between reconnect attempts.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(200);

/// A dialog waiting for an answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingDialog {
    /// Id within the session (`browser_dialog` names it).
    pub id: u64,
    /// `alert`, `confirm`, `prompt` or `beforeunload`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The dialog text.
    pub message: String,
    /// The page that opened it.
    pub url: String,
    #[serde(skip)]
    session: String,
}

/// The dialogs of one browser session.
#[derive(Debug, Default)]
pub struct Dialogs {
    next: AtomicU64,
    pending: Mutex<Vec<PendingDialog>>,
    handled: Mutex<Vec<String>>,
}

impl Dialogs {
    /// The dialogs waiting for an answer, oldest first.
    #[must_use]
    pub fn pending(&self) -> Vec<PendingDialog> {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// What the supervisor did with dialogs since the last call.
    #[must_use]
    pub fn take_handled(&self) -> Vec<String> {
        std::mem::take(&mut *self.handled.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn note(&self, line: String) {
        self.handled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(line);
    }

    fn take(&self, id: Option<u64>) -> Option<PendingDialog> {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let at = match id {
            Some(id) => pending.iter().position(|d| d.id == id)?,
            None if pending.is_empty() => return None,
            None => 0,
        };
        Some(pending.remove(at))
    }

    /// Answers dialog `id` (the oldest when `None`).
    ///
    /// # Errors
    ///
    /// `no pending dialog`, or the protocol error.
    pub async fn respond(
        &self,
        cdp: &Cdp,
        id: Option<u64>,
        accept: bool,
        prompt_text: Option<&str>,
    ) -> Result<(), String> {
        let dialog = self.take(id).ok_or("no pending dialog")?;
        answer(cdp, &dialog.session, accept, prompt_text).await
    }
}

async fn answer(
    cdp: &Cdp,
    session: &str,
    accept: bool,
    prompt_text: Option<&str>,
) -> Result<(), String> {
    let mut params = json!({"accept": accept});
    if let Some(text) = prompt_text {
        params["promptText"] = json!(text);
    }
    cdp.call("Page.handleJavaScriptDialog", params, Some(session))
        .await
        .map(|_| ())
}

struct State {
    endpoint: String,
    cdp: Mutex<Arc<Cdp>>,
    dialogs: Arc<Dialogs>,
    gave_up: AtomicBool,
    policy: DialogPolicy,
    dialog_timeout: Duration,
}

impl State {
    fn cdp(&self) -> Arc<Cdp> {
        Arc::clone(&self.cdp.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Watches one connection until dropped.
pub struct Supervisor {
    state: Arc<State>,
    task: tokio::task::JoinHandle<()>,
}

impl Supervisor {
    /// Starts supervising `cdp` with the session's dialog settings.
    #[must_use]
    pub fn start(cdp: Arc<Cdp>, policy: DialogPolicy, dialog_timeout_secs: u64) -> Self {
        let state = Arc::new(State {
            endpoint: cdp.endpoint().to_owned(),
            cdp: Mutex::new(cdp),
            dialogs: Arc::default(),
            gave_up: AtomicBool::new(false),
            policy,
            dialog_timeout: Duration::from_secs(dialog_timeout_secs),
        });
        let task = tokio::spawn(run(Arc::downgrade(&state)));
        Self { state, task }
    }

    /// The current connection (replaced after a reconnect).
    #[must_use]
    pub fn cdp(&self) -> Arc<Cdp> {
        self.state.cdp()
    }

    /// The session's dialogs.
    #[must_use]
    pub fn dialogs(&self) -> &Arc<Dialogs> {
        &self.state.dialogs
    }

    /// Whether reconnecting failed [`MAX_RECONNECTS`] times in a row.
    #[must_use]
    pub fn gave_up(&self) -> bool {
        self.state.gave_up.load(Ordering::SeqCst)
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(state: Weak<State>) {
    loop {
        let Some(cdp) = state.upgrade().map(|s| s.cdp()) else {
            return;
        };
        watch(&state, &cdp).await;
        let Some(strong) = state.upgrade() else {
            return;
        };
        let Some(fresh) = reconnect(&strong.endpoint).await else {
            strong.gave_up.store(true, Ordering::SeqCst);
            return;
        };
        *strong.cdp.lock().unwrap_or_else(PoisonError::into_inner) = fresh;
    }
}

/// Handles events of `cdp` until it closes.
async fn watch(state: &Weak<State>, cdp: &Arc<Cdp>) {
    let mut events = cdp.subscribe();
    loop {
        let event = tokio::select! {
            event = events.recv() => event,
            () = cdp.closed() => return,
        };
        match event {
            Ok(event) => {
                let Some(state) = state.upgrade() else {
                    return;
                };
                on_event(&state, cdp, &event).await;
            }
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return,
        }
    }
}

async fn reconnect(endpoint: &str) -> Option<Arc<Cdp>> {
    for _ in 0..MAX_RECONNECTS {
        tokio::time::sleep(RECONNECT_BACKOFF).await;
        if let Ok(cdp) = Cdp::connect(endpoint).await {
            return Some(cdp);
        }
    }
    None
}

async fn on_event(state: &Arc<State>, cdp: &Arc<Cdp>, event: &Event) {
    let Some(session) = event.session_id.clone() else {
        return;
    };
    match event.method.as_str() {
        "Page.javascriptDialogOpening" => opened(state, cdp, session, &event.params).await,
        "Page.javascriptDialogClosed" => {
            state
                .dialogs
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(|d| d.session != session);
        }
        _ => {}
    }
}

async fn opened(state: &Arc<State>, cdp: &Arc<Cdp>, session: String, params: &Value) {
    let text = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let dialog = PendingDialog {
        id: state.dialogs.next.fetch_add(1, Ordering::SeqCst) + 1,
        kind: text("type"),
        message: text("message"),
        url: text("url"),
        session,
    };
    let accept = match state.policy {
        DialogPolicy::AutoAccept => true,
        DialogPolicy::AutoDismiss => false,
        DialogPolicy::MustRespond => {
            schedule_dismissal(state, dialog.id);
            state
                .dialogs
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(dialog);
            return;
        }
    };
    let prompt = text("defaultPrompt");
    let prompt = (accept && dialog.kind == "prompt").then_some(prompt.as_str());
    let verb = if accept { "accepted" } else { "dismissed" };
    if answer(cdp, &dialog.session, accept, prompt).await.is_ok() {
        state
            .dialogs
            .note(format!("{verb} {} dialog: {}", dialog.kind, dialog.message));
    }
}

/// Dismisses dialog `id` if nobody answered it within the timeout.
fn schedule_dismissal(state: &Arc<State>, id: u64) {
    let weak = Arc::downgrade(state);
    let after = state.dialog_timeout;
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        let Some(state) = weak.upgrade() else {
            return;
        };
        let Some(dialog) = state.dialogs.take(Some(id)) else {
            return;
        };
        if answer(&state.cdp(), &dialog.session, false, None)
            .await
            .is_ok()
        {
            state.dialogs.note(format!(
                "dismissed {} dialog after {} s without an answer: {}",
                dialog.kind,
                after.as_secs(),
                dialog.message
            ));
        }
    });
}
