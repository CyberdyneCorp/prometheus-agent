//! The in-process line interface's pet, `/pet` and `/hatch` (spec: pets,
//! Terminal pet rendering; Pet slash command; Generating a pet; Affection
//! reactions): the turn's activity drives the pet pane, and the commands
//! run here without a model call or a change to the session.

use pmt_core::id::SessionId;
use pmt_pets::hatch::{Hatch, Step};
use serde_json::json;

use super::Repl;
use super::prompts::Pending;
use crate::chat::session::progress::Progress;
use crate::chat::theme::Role;
use crate::pets_host::slash::{self, Pet};

impl Repl {
    fn with_pet(&self, f: impl FnOnce(&crate::pets_host::line::LinePet)) {
        if let Some(pet) = &self.pet {
            f(pet);
        }
    }

    /// Tool activity of the running turn, for the pet.
    pub(super) fn pet_progress(&self, progress: &Progress) {
        self.with_pet(|pet| match progress {
            Progress::Call { tool, .. } => pet.event("tool.call", &json!({"tool": tool})),
            Progress::Result { tool, ok, text } => pet.event(
                "tool.result",
                &json!({"tool": tool, "ok": ok, "output": text}),
            ),
            Progress::Reasoning(_) => pet.event("assistant.reasoning", &json!({})),
            Progress::Assistant(_) | Progress::Notice(_) => {}
        });
    }

    /// A turn starts (`text`: what the user sent, for the heart).
    pub(super) fn pet_turn_started(&self, text: Option<&str>) {
        self.with_pet(|pet| {
            if let Some(text) = text {
                pet.sent(text);
            }
            pet.event("turn.start", &json!({}));
        });
    }

    /// The turn ended.
    pub(super) fn pet_turn_ended(&self, failed: bool) {
        self.with_pet(|pet| pet.turn_ended(failed));
    }

    /// An approval or clarify question waits (or not) for the user.
    pub(super) fn pet_sync_waiting(&self) {
        let waiting = matches!(
            self.pending,
            Some(Pending::Approval(_) | Pending::Clarify { .. })
        );
        self.with_pet(|pet| pet.waiting(u32::from(waiting)));
    }

    /// After a turn: the history is rescanned on a task of its own (the
    /// prompt never waits for it); each tier reached is posted to the
    /// chat and the pet jumps (spec: pets, Achievement unlocks
    /// celebrate). Nothing reaches the session.
    pub(super) fn achievements_after_turn(&self) {
        let (scope, store) = (
            self.factory.scope.clone(),
            std::sync::Arc::clone(&self.factory.store),
        );
        let posted = self.posted.clone();
        let pet = self
            .pet
            .as_ref()
            .map(crate::pets_host::line::LinePet::celebrator);
        drop(pmt_core::spawn::blocking(move || {
            for unlock in crate::pets_host::achievements::rescan(&scope, &store) {
                let data = json!({"id": unlock.id, "tier": unlock.tier});
                let _ = posted.send(crate::pets_host::achievements::unlocked_line(&data));
                if let Some(pet) = &pet {
                    pet.celebrate();
                }
            }
        }));
    }

    /// `/pet [list|off|scale <factor>|<slug>]`.
    pub(super) fn pet_command(&mut self, args: &str) {
        let scope = &self.factory.scope;
        let pet = Pet::parse(args);
        let shown = match pet {
            Pet::List => Ok(slash::list(scope)),
            other => slash::run(scope, &other),
        };
        match shown {
            Ok(text) => self.notice(&text),
            Err(err) => self.error(&err),
        }
        self.with_pet(crate::pets_host::line::LinePet::refresh);
    }

    /// `/hatch <description>` in `session` (which pays for the images):
    /// the backend first (none: its error, nothing spent), then the next
    /// lines answer each step.
    pub(super) fn hatch_command(&mut self, args: &str, session: SessionId) {
        if args.trim().is_empty() {
            self.notice("usage: /hatch <description>");
            return;
        }
        let scope = &self.factory.scope;
        let billing = crate::pets_host::meter::Billing::Session(session);
        let backend = match crate::pets_host::backend::select(scope, billing) {
            Ok(backend) => backend,
            Err(err) => return self.error(&err),
        };
        let drafts = scope.dir().join("cache/pets/hatch");
        let (hatch, step) = Hatch::start(args, backend, crate::pets_host::pets(scope), drafts);
        self.hatch_step(Box::new(hatch), step);
    }

    /// The next line answers `/hatch`'s question; `false` when none is
    /// asked.
    pub(super) async fn hatch_answer(&mut self, line: &str) -> bool {
        let Some(mut hatch) = self.hatch.take() else {
            return false;
        };
        let step = hatch.answer(line).await;
        self.hatch_step(hatch, step);
        true
    }

    fn hatch_step(&mut self, hatch: Box<Hatch>, step: Step) {
        match step {
            Step::Ask(text) => {
                self.say(Role::Notice, &text);
                self.hatch = Some(hatch);
            }
            Step::Done { text, failed, .. } => {
                self.say(if failed { Role::Error } else { Role::Notice }, &text);
            }
        }
    }
}
