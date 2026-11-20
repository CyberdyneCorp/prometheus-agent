//! `/language` in the TUI (spec: i18n, Language switchers show endonyms):
//! the picker lists `i18n.languages` by endonym and id; `Enter` writes
//! the choice to `display.language` with `config.patch`, and the TUI's
//! text follows within seconds (the strings re-read the profile's
//! configuration), without a restart.

use crossterm::event::{KeyCode, KeyEvent};
use pmt_core::error::Error;
use pmt_i18n::t;
use pmt_rpc::contracts::{
    ConfigChange, ConfigPatch, ConfigPatchParams, I18nLanguages, Languages, ProfileParams,
};
use pmt_tui::app::Overlay;
use pmt_tui::languages::{Choice, LanguagePicker};

use super::overlays::Step;
use super::screen::{Done, Tui};

fn rpc(err: &pmt_rpc::RpcError) -> Error {
    pmt_rpc::client::error_of(err)
}

impl Tui<'_> {
    /// `/language`: asks the daemon for the profile's languages; the
    /// picker opens when they arrive.
    pub(super) fn open_languages(&self) {
        let client = self.link.client.clone();
        let profile = self.open.scope.name().to_string();
        self.background(async move {
            let params = ProfileParams { profile };
            let listed = client.call_typed::<I18nLanguages>(&params).await;
            Done::Languages(listed.map_err(|e| rpc(&e)))
        });
    }

    /// `Done::Languages`: the picker, or why there is none.
    pub(super) fn languages_listed(&mut self, listed: Result<Languages, Error>) {
        match listed {
            Ok(languages) => {
                let choices = languages
                    .languages
                    .into_iter()
                    .map(|l| Choice {
                        id: l.id,
                        endonym: l.endonym,
                    })
                    .collect();
                let picker = LanguagePicker::new(choices, &languages.current);
                self.app.overlay = Some(Overlay::Languages(picker));
            }
            Err(err) => self.notice(err.message().to_owned()),
        }
    }

    pub(super) fn language_key(&mut self, picker: &mut LanguagePicker, key: &KeyEvent) -> Step {
        match key.code {
            KeyCode::Esc => return Step::Close,
            KeyCode::Up => picker.move_selection(-1),
            KeyCode::Down => picker.move_selection(1),
            KeyCode::Enter => {
                if let Some(choice) = picker.chosen() {
                    self.set_language(choice.id.clone());
                }
                return Step::Close;
            }
            _ => {}
        }
        Step::Stay
    }

    fn set_language(&self, id: String) {
        let client = self.link.client.clone();
        let profile = self.open.scope.name().to_string();
        self.background(async move {
            let params = ConfigPatchParams {
                profile,
                changes: vec![ConfigChange {
                    key: "display.language".to_owned(),
                    value: Some(serde_json::Value::String(id.clone())),
                }],
            };
            let patched = client.call_typed::<ConfigPatch>(&params).await;
            Done::LanguageSet(patched.map(|_| id).map_err(|e| rpc(&e)))
        });
    }

    /// `Done::LanguageSet`: says which language is now the profile's.
    pub(super) fn language_set(&mut self, set: Result<String, Error>) {
        match set {
            Ok(id) => {
                let line = t!(self.strings(), "tui.language_set", 0 = id);
                self.notice(line);
            }
            Err(err) => self.notice(err.message().to_owned()),
        }
    }
}
