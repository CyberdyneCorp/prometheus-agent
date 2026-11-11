//! Secret and sudo entry (spec: tui, Server-to-client requests): a card
//! whose input is masked. While it is on screen, keys and pastes go to its
//! own buffer instead of the composer, so the value is never drawn (the
//! card shows one bullet per character), never enters the history and
//! never reaches the transcript. Enter sends it; an empty input, Esc or
//! Ctrl+C declines (`None`).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pmt_i18n::t;
use pmt_rpc::contracts::{SecretAnswer, SudoAnswer};
use pmt_tui::app::Card;
use pmt_tui::keys::Action;
use serde_json::Value;

use super::screen::Tui;

/// The request for a secret's value.
pub(super) const SECRET: &str = "secret.request";
/// The request for the sudo password.
pub(super) const SUDO: &str = "sudo.request";

/// Whether requests of `method` are answered on a masked card.
pub(super) fn is_masked(method: &str) -> bool {
    method == SECRET || method == SUDO
}

/// The card of a secret or sudo request holding `typed` characters, with
/// the countdown `within` after the hint.
pub(super) fn card(
    strings: &pmt_i18n::Strings,
    method: &str,
    p: &Value,
    typed: usize,
    within: &str,
) -> Card {
    let (title, why) = if method == SECRET {
        let name = p["name"].as_str().unwrap_or_default();
        (
            t!(strings, "tui.secret_needed", 0 = name),
            p["reason"].as_str().map(str::to_owned),
        )
    } else {
        (
            t!(strings, "tui.sudo_needed"),
            p["command"]
                .as_str()
                .map(|c| t!(strings, "tui.sudo_for", 0 = c)),
        )
    };
    let input = format!("› {}", "•".repeat(typed));
    let body = why.map_or_else(|| input.clone(), |why| format!("{why}\n{input}"));
    Card {
        title,
        body,
        hint: format!("{}{within}", strings.get("tui.masked_hint")),
    }
}

impl Tui<'_> {
    /// The card on screen takes masked input.
    pub(super) fn masked_front(&self) -> bool {
        self.asking.front().is_some_and(|a| is_masked(a.method()))
    }

    /// A key while a masked card is on screen: it never reaches the
    /// composer; scrolling still works.
    pub(super) fn masked_key(&mut self, key: &KeyEvent) {
        match self.open.bindings.action(key) {
            Some(Action::Submit) => self.send_masked(false),
            Some(Action::Interrupt) => self.send_masked(true),
            Some(Action::ClearInput) => self.masked.clear(),
            Some(
                scroll
                @ (Action::ScrollUp | Action::ScrollDown | Action::PageUp | Action::PageDown),
            ) => self.on_action(scroll),
            Some(_) => {}
            None => match key.code {
                KeyCode::Esc => self.send_masked(true),
                KeyCode::Backspace => {
                    self.masked.pop();
                }
                KeyCode::Char(c) if key.modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
                    self.masked.push(c);
                }
                _ => {}
            },
        }
        self.show_question();
    }

    /// A paste while a masked card is on screen (line breaks dropped).
    pub(super) fn masked_paste(&mut self, text: &str) {
        self.masked
            .extend(text.chars().filter(|c| !matches!(c, '\n' | '\r')));
        self.show_question();
    }

    /// Answers the masked card: what was typed, or `None` when `decline`
    /// or nothing was typed.
    fn send_masked(&mut self, decline: bool) {
        let Some(asked) = self.asking.pop_front() else {
            return;
        };
        let typed = std::mem::take(&mut self.masked);
        let value = (!decline && !typed.is_empty()).then_some(typed);
        let sent = value.is_some();
        let strings = self.strings();
        let (answer, said) = if asked.method() == SECRET {
            let name = asked.call.params["name"].as_str().unwrap_or_default();
            let key = if sent {
                "tui.secret_sent"
            } else {
                "tui.secret_declined"
            };
            (
                serde_json::to_value(SecretAnswer { value }),
                strings.fmt(key, &[("0", &name)]),
            )
        } else {
            let key = if sent {
                "tui.sudo_sent"
            } else {
                "tui.sudo_declined"
            };
            (
                serde_json::to_value(SudoAnswer { password: value }),
                strings.get(key).to_owned(),
            )
        };
        if let Ok(answer) = answer {
            self.link.client.respond(&asked.call.id, Ok(answer));
        }
        self.notice(said);
        self.show_question();
    }
}
