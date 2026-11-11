//! Session automation in the line interface (spec: session-automation,
//! Idle-boundary arbitration; Automated turns are plain user turns): the
//! REPL drives the automation of the session it runs in-process. When a
//! turn ends, [`boundary::after_turn`] may hand back a goal continuation;
//! at the idle prompt a timer wakes for what [`boundary::due`] reserves.
//! A line the person typed always goes first: a continuation waiting
//! behind it is dropped (the judge looks again after that turn), and a
//! timer wakeup only fires when nothing is queued.

use pmt_agent::turn::{ExitReason, TurnInput, TurnResult};
use pmt_automation::Kind;
use pmt_core::error::Error;

use super::Repl;
use crate::automation::boundary::{self, Ended, Reserved, Session};
use crate::automation::{self, Host};
use crate::chat::session::Live;
use crate::chat::theme::Role;

/// What a finished turn tells the boundary.
pub(super) struct Outcome {
    origin: Option<Kind>,
    reply: String,
    interrupted: bool,
}

impl Outcome {
    pub(super) fn of(origin: Option<&str>, result: &Result<TurnResult, Error>) -> Self {
        let (reply, interrupted) = match result {
            Ok(result) => (
                result.final_response.clone().unwrap_or_default(),
                result.exit_reason == ExitReason::InterruptedByUser,
            ),
            Err(_) => (String::new(), false),
        };
        Self {
            origin: origin.and_then(Kind::parse),
            reply,
            interrupted,
        }
    }
}

impl Repl {
    /// After a turn: the loop and goal boundaries; a continuation waits in
    /// [`Repl::wakeup`] for the next idle moment.
    pub(super) async fn automation_after(&mut self, live: &Live, outcome: Outcome) {
        let session = Session {
            factory: &self.factory,
            live,
            host: Host::Line,
        };
        let ended = Ended {
            origin: outcome.origin,
            reply: &outcome.reply,
            interrupted: outcome.interrupted,
        };
        let next = boundary::after_turn(&session, ended).await;
        for notice in &next.notices {
            self.notice(notice);
        }
        self.wakeup = next.wakeup.map(Reserved::continuation);
    }

    /// When the idle prompt should wake for automation.
    pub(super) fn automation_deadline(&self) -> Option<tokio::time::Instant> {
        let at = boundary::next_check(&self.factory.store, self.live().id, Host::Line)?;
        let wait = u64::try_from(at - automation::now_ms()).unwrap_or(0);
        Some(tokio::time::Instant::now() + std::time::Duration::from_millis(wait))
    }

    /// The idle prompt's automation timer fired: reserve what is due.
    pub(super) fn automation_due(&mut self) {
        let scope = self.live().config.get();
        let limits = automation::limits(scope.config());
        let strings = self.strings();
        let (reserved, notices) = boundary::due(
            &*strings,
            &self.factory.store,
            self.live().id,
            Host::Line,
            &limits,
        );
        for notice in &notices {
            self.notice(notice);
        }
        self.wakeup = reserved;
    }

    /// Gives back a reserved wakeup a typed line preempted.
    pub(super) fn drop_wakeup(&mut self) {
        if let Some(reserved) = self.wakeup.take() {
            boundary::refund(&self.factory.store, self.live().id, reserved);
        }
    }

    /// Runs an automated turn at an idle boundary; refused before it
    /// starts (a paused home), its reservation is given back.
    pub(super) async fn automated(&mut self, reserved: Reserved) {
        if let Some(pause) = pmt_core::pause::active(self.factory.scope.home()) {
            boundary::refund(&self.factory.store, self.live().id, reserved);
            self.say(Role::Error, &pause.notice(&*self.strings()));
            return;
        }
        let wakeup = reserved.wakeup;
        if let Some(notice) = &wakeup.notice {
            self.notice(notice);
        }
        if wakeup.prompt.starts_with('/') {
            // A loop whose wakeup is a slash command runs it; the wakeup
            // still counts (and may end the loop).
            Box::pin(self.slash(wakeup.prompt.clone())).await;
            if let Some(live) = self.live.take() {
                let outcome = Outcome {
                    origin: Some(wakeup.kind),
                    reply: String::new(),
                    interrupted: false,
                };
                Box::pin(self.automation_after(&live, outcome)).await;
                self.live = Some(live);
            }
            return;
        }
        let input = TurnInput {
            origin: Some(wakeup.kind.as_str().to_owned()),
            ..TurnInput::text(wakeup.prompt)
        };
        Box::pin(self.run_input(input)).await;
    }

    /// The automation rows for the status line.
    pub(super) fn automation_rows(&self) -> Vec<String> {
        let scope = self.live().config.get();
        let limits = automation::limits(scope.config());
        automation::rows(
            &*self.strings(),
            &self.factory.store,
            self.live().id,
            &limits,
        )
    }
}
