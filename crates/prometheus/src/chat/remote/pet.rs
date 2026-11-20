//! The attached line interface's `/pet` and `/hatch` (spec: pets, Pet
//! slash command; Generating a pet): run in this client, never sent to the
//! daemon's session.

use pmt_pets::hatch::{Hatch, Step};

use super::line::Chat;
use super::render::Out as _;
use crate::chat::theme::Role;
use crate::pets_host::slash::{self, Pet};

impl Chat<'_, '_> {
    /// `/pet [list|off|scale <factor>|<slug>]`.
    pub(super) fn pet_command(&mut self, args: &str) {
        let scope = self.start.scope;
        let shown = match Pet::parse(args) {
            Pet::List => Ok(slash::list(scope)),
            other => slash::run(scope, &other),
        };
        match shown {
            Ok(text) => self.screen.notice(&text),
            Err(err) => self.screen.error(&err),
        }
        if let Some(pet) = &self.screen.pet {
            pet.refresh();
        }
    }

    /// `/hatch <description>`: the backend first (none: its error,
    /// nothing spent), then the next lines answer each step.
    pub(super) fn hatch_command(&mut self, args: &str) {
        if args.trim().is_empty() {
            self.screen.notice("usage: /hatch <description>");
            return;
        }
        let scope = self.start.scope;
        let backend = match crate::pets_host::backend::select(scope) {
            Ok(backend) => backend,
            Err(err) => return self.screen.error(&err),
        };
        let drafts = scope.dir().join("cache/pets/hatch");
        let (hatch, step) = Hatch::start(args, backend, crate::pets_host::pets(scope), drafts);
        self.hatch_step(Box::new(hatch), step);
    }

    /// The line answers `/hatch`'s question; `false` when none is asked.
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
                self.screen.say(Role::Notice, &text);
                self.hatch = Some(hatch);
            }
            Step::Done { text, failed, .. } => {
                self.screen
                    .say(if failed { Role::Error } else { Role::Notice }, &text);
            }
        }
    }
}
