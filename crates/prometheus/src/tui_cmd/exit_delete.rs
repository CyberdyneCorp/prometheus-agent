//! `/exit --delete` in the TUI (spec: cli, Exit with delete and profile
//! archive slash forms; Destructive command confirmation): the card asks
//! `once / always / cancel` first, as the line interfaces do, unless the
//! command carries `--yes` or `cli.confirm_destructive` is off; `always`
//! turns that setting off for the profile.

use pmt_i18n::Surface;
use pmt_tui::app::Card;

use super::Ending;
use super::screen::Tui;
use crate::slash::{Confirmation, confirmation};

/// Whether `/exit --delete` asks first, and whether it is asking now.
pub(super) struct ExitDelete {
    /// `cli.confirm_destructive`, until `always` turns it off.
    pub(super) confirm: bool,
    /// The card waits for `once`, `always` or `cancel`.
    pub(super) asked: bool,
}

impl Tui<'_> {
    /// `/exit --delete [--yes]`: leaves and deletes the session, after the
    /// confirmation card when one is due.
    pub(super) fn quit_deleting(&mut self, args: &str) {
        let yes = args.split_whitespace().any(|w| matches!(w, "--yes" | "-y"));
        if yes || !self.exit_delete.confirm {
            self.ending = Some(Ending::Delete(self.link.session.clone()));
            return;
        }
        self.exit_delete.asked = true;
        self.show_question();
    }

    /// The card asking before the session is deleted, while it waits.
    pub(super) fn delete_card(&self) -> Option<Card> {
        let strings = pmt_config::language::strings(self.open.scope.dir(), Surface::Core);
        self.exit_delete.asked.then(|| Card {
            title: pmt_i18n::t!(
                strings,
                "cli.confirm_destructive",
                command = "exit --delete"
            ),
            body: String::new(),
            hint: String::new(),
        })
    }

    /// The answer to the delete card: every line answers it (anything but
    /// `once` or `always` cancels, as in the line interfaces).
    pub(super) fn answer_delete(&mut self, line: &str) -> bool {
        self.exit_delete.asked = false;
        let choice = confirmation(line);
        if choice == Confirmation::Always {
            self.exit_delete.confirm = false;
            let file = self.open.scope.profile().config_file();
            if let Err(err) = file.set("cli.confirm_destructive", "false") {
                self.error(&err);
            }
        }
        if choice == Confirmation::Cancel {
            let strings = pmt_config::language::strings(self.open.scope.dir(), Surface::Core);
            self.notice(pmt_i18n::t!(strings, "cli.exit_delete_cancelled"));
        } else {
            self.ending = Some(Ending::Delete(self.link.session.clone()));
        }
        self.show_question();
        true
    }
}
