//! The chat's model (spec: messaging-gateway, Interactive model picker and
//! chat model override). `/model` without arguments shows a picker: on
//! adapters with buttons a card of the configured providers, then the
//! chosen provider's models, at most 8 per page with paging buttons, each
//! edited in place; elsewhere a numbered list answered with `/model <n>`.
//! A chosen model (picked, numbered or typed) switches the session as
//! `/model <ref>` does and becomes the chat's override in `state.db`:
//! every new session in the chat starts on it (it outlives `/new`) until
//! `/model --clear`. `/model status` adds the override to the registry's
//! answer.

use std::fmt::Write as _;

use pmt_rpc::contracts::Dispatched;
use pmt_store::gateway::outbox::OutboundKind;
use serde_json::json;

use super::Lane;
use crate::messaging::adapter::Button;
use crate::messaging::agents::ModelChoice;
use crate::messaging::cards::CardKind;
use crate::messaging::descriptor::Interactive;
use crate::messaging::key::Routing;

/// The id prefix of a picker card (paging buttons tell it by this).
pub(super) const PICKER: &str = "mdl";
/// Providers or models on one page of the picker.
const PER_PAGE: usize = 8;
/// Pickers whose buttons still answer (older cards go stale).
const KEPT: usize = 4;

/// What `/model --clear` answers.
pub(crate) const CLEARED: &str =
    "Chat model override cleared; new sessions here use the profile's model.";

/// One picker card: its id and the choices it was drawn from.
#[derive(Debug, Clone)]
pub(super) struct Picker {
    id: String,
    choices: Vec<ModelChoice>,
}

/// What a picker button asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Press {
    /// A page of providers (1-based).
    Providers(usize),
    /// A page of a provider's models.
    Models(usize, usize),
    /// A model of a provider.
    Select(usize, usize),
}

impl Press {
    fn choice(self) -> String {
        match self {
            Self::Providers(page) => format!("L{page}"),
            Self::Models(provider, page) => format!("P{provider}.{page}"),
            Self::Select(provider, model) => format!("S{provider}.{model}"),
        }
    }

    fn parse(choice: &str) -> Option<Self> {
        let pair = |rest: &str| -> Option<(usize, usize)> {
            let (a, b) = rest.split_once('.')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        };
        let (tag, rest) = choice.split_at_checked(1)?;
        match tag {
            "L" => rest.parse().ok().map(Self::Providers),
            "P" => pair(rest).map(|(p, n)| Self::Models(p, n)),
            "S" => pair(rest).map(|(p, m)| Self::Select(p, m)),
            _ => None,
        }
    }
}

/// `<provider>/<model>` for every choice, in order (the numbered list).
fn numbered(choices: &[ModelChoice]) -> Vec<String> {
    choices
        .iter()
        .flat_map(|c| c.models.iter().map(move |m| format!("{}/{m}", c.provider)))
        .collect()
}

/// The page `page` of `items` and the page count (`page` clamped).
fn page<T: Clone>(items: &[T], page: usize) -> (Vec<(usize, T)>, usize, usize) {
    let pages = items.len().div_ceil(PER_PAGE).max(1);
    let page = page.clamp(1, pages);
    let shown = items
        .iter()
        .cloned()
        .enumerate()
        .skip((page - 1) * PER_PAGE)
        .take(PER_PAGE)
        .collect();
    (shown, page, pages)
}

/// The model reference `/model <args>` names (no flags), and whether it
/// lasts beyond the next turn (`--once` does not).
fn reference_of(args: &str) -> Option<String> {
    let words: Vec<&str> = args.split_whitespace().collect();
    if words.contains(&"--once") {
        return None;
    }
    let mut skip = false;
    for word in words {
        if skip {
            skip = false;
            continue;
        }
        if matches!(word, "--provider" | "--reasoning") {
            skip = true;
            continue;
        }
        if !word.starts_with("--") {
            return Some(word.to_owned());
        }
    }
    None
}

impl Lane {
    /// `/model [status | --clear | <n> | <ref> ...]` on the gateway.
    pub(super) async fn model_command(&mut self, routing: &Routing, args: &str) {
        let args = args.trim();
        match args {
            "" => {
                let choices = self.shared.agents.model_choices(&self.profile);
                if choices.is_empty() {
                    // Nothing to pick from: the session's route, as `/model` says.
                    let current = self.registry_answer(routing, "/model").await;
                    self.say(&current);
                } else {
                    self.model_picker(choices);
                }
            }
            "--clear" => {
                self.set_chat_model(routing, None);
                self.say(CLEARED);
            }
            "status" => self.model_status(routing).await,
            _ => {
                if let Ok(n) = args.parse::<usize>() {
                    self.numbered_model(routing, n).await;
                } else {
                    self.switch_model(routing, args).await;
                }
            }
        }
    }

    /// The chat's model override, if any.
    pub(super) fn chat_model(&self, routing: &Routing) -> Option<String> {
        let store = self.shared.store(&self.profile)?;
        store
            .chat_prefs(&routing.platform, &routing.chat_id)
            .ok()
            .and_then(|p| p.model)
    }

    fn set_chat_model(&self, routing: &Routing, model: Option<&str>) {
        if let Some(store) = self.shared.store(&self.profile)
            && let Err(err) = store.set_chat_model(&routing.platform, &routing.chat_id, model)
        {
            tracing::warn!("chat model override not stored: {err}");
        }
    }

    /// `/model status`: the session's route (the registry's answer) and
    /// the chat's override.
    async fn model_status(&mut self, routing: &Routing) {
        let current = self.registry_answer(routing, "/model").await;
        let over = self.chat_model(routing).map_or_else(
            || "none (new sessions use the profile's model)".to_owned(),
            |m| format!("{m} (new sessions here start on it; /model --clear removes it)"),
        );
        self.say(&format!("{current}\nChat override: {over}"));
    }

    /// Switches the session as `/model <args>` does; on success the named
    /// model becomes the chat's override.
    pub(super) async fn switch_model(&mut self, routing: &Routing, args: &str) {
        if self.running.is_some() {
            self.say(&super::commands::rejected("model"));
            return;
        }
        let session = match self.ensure_session(routing).await {
            Ok(session) => session,
            Err(err) => {
                self.say(err.message());
                return;
            }
        };
        let session_link = self.shared.server.link(&session);
        let command = format!("/model {args}");
        let outcome = self
            .shared
            .agents
            .run_command(&self.profile, &session, &command, session_link)
            .await;
        match outcome {
            Ok(Dispatched::Done { notice }) => {
                let mut text = notice;
                if let Some(reference) = reference_of(args) {
                    self.set_chat_model(routing, Some(&reference));
                    let _ = write!(
                        text,
                        "\nNew sessions in this chat use {reference} (/model --clear to undo)."
                    );
                }
                self.say(text.trim());
            }
            Ok(Dispatched::Refused { reason }) => self.say(&reason),
            Ok(Dispatched::Send { .. }) => {}
            Err(err) => self.say(err.message()),
        }
    }

    /// What a registry command answers, as text (errors included).
    async fn registry_answer(&mut self, routing: &Routing, command: &str) -> String {
        let session = match self.ensure_session(routing).await {
            Ok(session) => session,
            Err(err) => return err.message().to_owned(),
        };
        let session_link = self.shared.server.link(&session);
        match self
            .shared
            .agents
            .run_command(&self.profile, &session, command, session_link)
            .await
        {
            Ok(Dispatched::Done { notice }) => notice,
            Ok(Dispatched::Refused { reason }) => reason,
            Ok(Dispatched::Send { message, .. }) => message,
            Err(err) => err.message().to_owned(),
        }
    }

    /// `/model <n>`: the n-th model of the numbered list.
    async fn numbered_model(&mut self, routing: &Routing, n: usize) {
        let all = numbered(&self.shared.agents.model_choices(&self.profile));
        match n.checked_sub(1).and_then(|i| all.get(i)) {
            Some(reference) => self.switch_model(routing, &reference.clone()).await,
            None => self.say(&format!(
                "There is no model {n}; /model lists {} models.",
                all.len()
            )),
        }
    }

    /// Bare `/model`: a picker card, or a numbered list without buttons.
    fn model_picker(&mut self, choices: Vec<ModelChoice>) {
        if self.tier() != Interactive::Native {
            let mut text = String::from("Models (answer with /model <n>):");
            for (i, reference) in numbered(&choices).iter().enumerate() {
                let _ = write!(text, "\n{}. {reference}", i + 1);
            }
            self.say(&text);
            return;
        }
        let id = format!("{PICKER}-{}", crate::tickets::random_hex(6));
        self.pickers.push(Picker {
            id: id.clone(),
            choices,
        });
        if self.pickers.len() > KEPT {
            self.pickers.remove(0);
        }
        self.picker_card(&id, Press::Providers(1), true);
    }

    /// A picker button was pressed (its signature verified).
    pub(super) fn picker_pressed(&mut self, routing: &Routing, id: &str, choice: &str) {
        let Some(press) = Press::parse(choice) else {
            return;
        };
        if !self.pickers.iter().any(|p| p.id == id) {
            self.say("This model picker has expired; send /model again.");
            return;
        }
        if let Press::Select(provider, model) = press {
            let reference = self
                .pickers
                .iter()
                .find(|p| p.id == id)
                .and_then(|p| p.choices.get(provider))
                .and_then(|c| Some(format!("{}/{}", c.provider, c.models.get(model)?)));
            let Some(reference) = reference else {
                return;
            };
            self.close_picker(id, &format!("Model: {reference}"));
            if let Some(tx) = self.tx.upgrade() {
                let _ = tx.send(super::Msg::ModelPicked {
                    reference,
                    routing: Box::new(routing.clone()),
                });
            }
            return;
        }
        self.picker_card(id, press, false);
    }

    /// Shows (or edits in place) picker `id` at `press`.
    fn picker_card(&self, id: &str, press: Press, first: bool) {
        let Some(picker) = self.pickers.iter().find(|p| p.id == id) else {
            return;
        };
        let key = &self.shared.card_key;
        let button = |label: String, press: Press| Button {
            label,
            data: key.data(CardKind::Page, id, &press.choice()),
        };
        let (text, mut buttons, paging) = match press {
            Press::Providers(n) => {
                let (shown, n, pages) = page(&picker.choices, n);
                let buttons: Vec<Button> = shown
                    .into_iter()
                    .map(|(i, c)| button(c.provider, Press::Models(i, 1)))
                    .collect();
                let paging = (n, pages, Press::Providers(n - 1), Press::Providers(n + 1));
                ("Choose a provider:".to_owned(), buttons, paging)
            }
            Press::Models(provider, n) | Press::Select(provider, n) => {
                let Some(choice) = picker.choices.get(provider) else {
                    return;
                };
                let (shown, n, pages) = page(&choice.models, n);
                let mut buttons: Vec<Button> = shown
                    .into_iter()
                    .map(|(i, m)| button(m, Press::Select(provider, i)))
                    .collect();
                buttons.push(button("\u{2039} Providers".to_owned(), Press::Providers(1)));
                let paging = (
                    n,
                    pages,
                    Press::Models(provider, n - 1),
                    Press::Models(provider, n + 1),
                );
                (
                    format!("Choose a {} model:", choice.provider),
                    buttons,
                    paging,
                )
            }
        };
        let (n, pages, previous, next) = paging;
        if n > 1 {
            buttons.push(button("\u{2039} Previous".to_owned(), previous));
        }
        if n < pages {
            buttons.push(button("Next \u{203a}".to_owned(), next));
        }
        let body = json!({"text": text, "buttons": buttons}).to_string();
        self.picker_deliver(id, body, first);
    }

    fn close_picker(&self, id: &str, text: &str) {
        let body = json!({"text": text, "buttons": []}).to_string();
        self.picker_deliver(id, body, false);
    }

    /// Sends the picker's card, or edits it once it was sent.
    fn picker_deliver(&self, id: &str, body: String, first: bool) {
        let key = format!("page:{id}");
        if first {
            self.deliver(key, OutboundKind::Approval, body, None);
            return;
        }
        let edit_of = self
            .shared
            .store(&self.profile)
            .and_then(|store| store.outbound_message_id(&key).ok().flatten());
        let entry = format!("{key}:{}", crate::tickets::random_hex(4));
        self.deliver(entry, OutboundKind::Approval, body, edit_of);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presses_round_trip_through_their_choice() {
        for press in [
            Press::Providers(2),
            Press::Models(1, 3),
            Press::Select(0, 7),
        ] {
            assert_eq!(Press::parse(&press.choice()), Some(press));
        }
        assert_eq!(Press::parse("X1"), None);
        assert_eq!(Press::parse(""), None);
    }

    #[test]
    fn the_override_is_the_named_model_unless_it_is_once() {
        assert_eq!(
            reference_of("anthropic/claude-sonnet-4.5").as_deref(),
            Some("anthropic/claude-sonnet-4.5")
        );
        assert_eq!(
            reference_of("--provider openrouter gpt-5 --session").as_deref(),
            Some("gpt-5")
        );
        assert_eq!(reference_of("fast --once"), None);
    }

    #[test]
    fn pages_hold_eight() {
        let items: Vec<u32> = (0..19).collect();
        let (shown, n, pages) = page(&items, 3);
        assert_eq!((shown.len(), n, pages), (3, 3, 3));
        assert_eq!(shown[0], (16, 16));
        assert_eq!(page(&items, 9).1, 3, "clamped");
    }
}
