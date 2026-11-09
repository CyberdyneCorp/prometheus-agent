//! Turns a crash left unfinished, in daemon sessions (spec:
//! agent-turn-loop, Crash resume mid-tool-batch). When a session loads,
//! its unfinished turn becomes the session's first job: `auto` resumes it,
//! `never` closes it as crashed and `ask` (the default) first asks the
//! session's clients with a `clarify.request` (the TUI, the desktop and a
//! desktop answer those). The question holds the session busy, so the
//! message that loaded it waits behind the answer instead of closing the
//! turn.
//!
//! A messaging conversation is asked by the gateway's own restart recovery
//! instead (spec: messaging-gateway, Crash recovery and clean shutdown
//! marker; Discarding a restart-interrupted turn): its chat is told to
//! reply `/retry`, and its input waits until the user chooses. `/retry`
//! resumes the journaled turn here (finished results reused, unknown
//! outcomes reported rather than repeated); `/discard`, or the hold's
//! expiry, declines, and the next turn closes it as crashed.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use pmt_agent::turn::{TurnInput, Unfinished};
use pmt_config::schema::ResumeOnCrash;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_gateway::Link;
use pmt_rpc::contracts::{Admission, ClarifyAsk, ClarifyRequestParams, Dispatched, Submitted};
use serde_json::{Value, json};

use super::turns::Queued;
use super::{DaemonHost, Loaded, Profile};

/// The choice that resumes the turn.
const RESUME: &str = "Resume";
/// The choice that closes it as crashed.
const CLOSE: &str = "Close";

/// What becomes of an unfinished turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AfterCrash {
    Resume,
    Close,
    /// The session's clients decide.
    Ask,
}

impl From<ResumeOnCrash> for AfterCrash {
    fn from(setting: ResumeOnCrash) -> Self {
        match setting {
            ResumeOnCrash::Auto => Self::Resume,
            ResumeOnCrash::Never => Self::Close,
            ResumeOnCrash::Ask => Self::Ask,
        }
    }
}

impl DaemonHost {
    /// Queues the unfinished turn of a session that just loaded as its
    /// first job, per `agent.resume_on_crash`.
    pub(super) fn after_crash(
        &self,
        profile: Arc<Profile>,
        id: SessionId,
        session: &Arc<Loaded>,
        link: &Link,
    ) {
        let unfinished = match session.live.agent.unfinished_turn() {
            Ok(Some(unfinished)) => unfinished,
            Ok(None) => return,
            Err(err) => return tracing::warn!(error = %err, "turn journal unreadable"),
        };
        let choice = AfterCrash::from(profile.scope().config().agent.resume_on_crash);
        if choice == AfterCrash::Ask && session.chat.is_some() {
            // The gateway's restart recovery asks in the chat (`/retry`).
            return;
        }
        if let Err(err) = self.run_crashed(profile, id, session, (unfinished, choice), link) {
            tracing::warn!(error = %err, "crashed turn not resumed");
        }
    }

    /// Runs the crashed turn as the session's next job: at once when it
    /// is idle, else first in its queue.
    fn run_crashed(
        &self,
        profile: Arc<Profile>,
        id: SessionId,
        session: &Arc<Loaded>,
        (unfinished, choice): (Unfinished, AfterCrash),
        link: &Link,
    ) -> Result<Submitted, Error> {
        let job = Queued {
            input: TurnInput::default(),
            turn_id: unfinished.turn_id.clone(),
            run_budget: None,
            link: link.clone(),
            from_lane: false,
            resume: Some((unfinished, choice)),
        };
        let turn_id = job.turn_id.clone();
        let admission = if session.busy.swap(true, Ordering::AcqRel) {
            session
                .queue
                .lock()
                .expect("queue poisoned")
                .push_front(job);
            Admission::Queued
        } else {
            self.start(profile, id, Arc::clone(session), job)?;
            Admission::Started
        };
        Ok(Submitted {
            turn_id,
            admission,
            notice: None,
        })
    }

    /// Submitting the message a `/retry` of a crashed turn sent back
    /// resumes that turn; `None` for any other input.
    pub(super) fn resume_on_retry(
        &self,
        profile: &Arc<Profile>,
        (id, session): (SessionId, &Arc<Loaded>),
        text: &str,
        link: &Link,
    ) -> Option<Result<Submitted, Error>> {
        let retried = session.retry.lock().expect("retry poisoned").take()?;
        if retried != text.trim() {
            return None;
        }
        let unfinished = session.live.agent.unfinished_turn().ok().flatten()?;
        let resume = (unfinished, AfterCrash::Resume);
        Some(self.run_crashed(Arc::clone(profile), id, session, resume, link))
    }
}

/// `/retry` of a session whose last turn a crash left unfinished: sends
/// back its last message (the client or chat submits it, and that
/// resumes the turn) without undoing the turn, whose finished tool
/// results the resume reuses; `None` when no turn is unfinished.
pub(super) fn retry(session: &Loaded) -> Option<Dispatched> {
    session.live.agent.unfinished_turn().ok().flatten()?;
    let last = crate::chat::commands::session::last_user_input(&session.factory, session.live.id)
        .ok()
        .flatten()?;
    let message = last.text.trim().to_owned();
    *session.retry.lock().expect("retry poisoned") = Some(message.clone());
    Some(Dispatched::Send {
        message,
        images: Vec::new(),
    })
}

/// Whether the turn resumes: `Ask` asks the session's clients. Only a
/// `Resume` pick (or a typed yes) resumes; a decline closes the turn as
/// crashed, and so does a question nobody answers (like the line
/// interface's y/N prompt, which defaults to no).
pub(super) async fn decide(link: &Link, unfinished: &Unfinished, choice: AfterCrash) -> bool {
    match choice {
        AfterCrash::Resume => true,
        AfterCrash::Close => false,
        AfterCrash::Ask => {
            let params = question(link.session_id(), unfinished);
            let answer = link.ask::<ClarifyAsk>(&params, None).await;
            let resume = answer.is_some_and(|a| resumes(a.answers.first()));
            tracing::info!(resume, "crashed turn: the user was asked");
            resume
        }
    }
}

/// The question about `unfinished`.
fn question(session_id: &str, unfinished: &Unfinished) -> ClarifyRequestParams {
    let tools = unfinished.interrupted_tools();
    let running = if tools.is_empty() {
        String::new()
    } else {
        format!(" while {} ran", tools.join(", "))
    };
    ClarifyRequestParams {
        session_id: session_id.to_owned(),
        questions: vec![json!({
            "question": format!(
                "The last turn of this session stopped when the process ended{running}. \
                 Resume it? Finished tool results are reused; a call whose effect is \
                 unknown is reported, not repeated. Close ends it as crashed."
            ),
            "choices": [RESUME, CLOSE],
        })],
        allow_other: true,
        deadline_ms: None,
    }
}

/// A clarify answer that picks `Resume` or types yes.
fn resumes(answer: Option<&Value>) -> bool {
    let Some(answer) = answer else {
        return false;
    };
    let picked = answer["choices"]
        .as_array()
        .is_some_and(|c| c.iter().any(|c| c.as_str() == Some(RESUME)));
    let typed = answer["text"]
        .as_str()
        .is_some_and(|t| matches!(t.trim().to_lowercase().as_str(), "y" | "yes" | "resume"));
    picked || typed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_resume_pick_or_a_typed_yes_resumes() {
        let pick = |c: &str| json!({"status": "answered", "choices": [c]});
        let typed = |t: &str| json!({"status": "answered", "text": t});
        assert!(resumes(Some(&pick(RESUME))));
        assert!(resumes(Some(&typed(" Yes "))));
        assert!(!resumes(Some(&pick(CLOSE))));
        assert!(!resumes(Some(&typed("no"))));
        assert!(!resumes(Some(&json!({"status": "skipped"}))));
        assert!(!resumes(None));
    }
}
