//! Paged listings in a chat (spec: messaging-gateway, Platform start pings
//! and paginated command browser): `/commands [page]` lists every command
//! and skill command the sender may run on the gateway surface, 15 per
//! page on Telegram and 20 elsewhere. Adapters with buttons get signed
//! previous/next buttons that edit the listing in place; others get a
//! `page <n>/<total>` footer and page with `/commands <n>`.

use std::fmt::Write as _;

use pmt_store::gateway::outbox::OutboundKind;
use serde_json::json;

use super::Lane;
use crate::messaging::adapter::Button;
use crate::messaging::auth;
use crate::messaging::cards::CardKind;
use crate::messaging::descriptor::Interactive;
use crate::messaging::key::Routing;

/// The listing `/commands` shows (the paging buttons' id prefix).
const COMMANDS: &str = "cmds";

/// Entries per page of `/commands` on `platform`.
fn per_page(platform: &str) -> usize {
    if platform == "telegram" { 15 } else { 20 }
}

/// What asking for a page that does not exist answers.
pub(crate) fn no_such_page(page: usize, pages: usize) -> String {
    let plural = if pages == 1 { "" } else { "s" };
    format!("page {page} does not exist ({pages} page{plural})")
}

/// Page `page` (1-based) of `entries`, and the page count; `Err` with the
/// refusal when the page does not exist.
pub(crate) fn page_of(
    entries: &[String],
    page: usize,
    per_page: usize,
) -> Result<(Vec<String>, usize), String> {
    let pages = entries.len().div_ceil(per_page).max(1);
    if page == 0 || page > pages {
        return Err(no_such_page(page, pages));
    }
    let start = (page - 1) * per_page;
    let shown = entries.iter().skip(start).take(per_page).cloned().collect();
    Ok((shown, pages))
}

/// A requested page number: none is page 1, anything not a positive
/// number is refused.
fn requested(args: &str) -> Result<usize, String> {
    let args = args.trim();
    if args.is_empty() {
        return Ok(1);
    }
    args.parse::<usize>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| "Usage: /commands [page]".to_owned())
}

impl Lane {
    /// `/commands [page]`.
    pub(super) fn commands_command(&mut self, routing: &Routing, args: &str) {
        match requested(args) {
            Ok(page) => self.commands_page(routing, page, None),
            Err(usage) => self.say(&usage),
        }
    }

    /// The commands `routing`'s sender may run here, one line each.
    fn available_commands(&self, routing: &Routing) -> Vec<String> {
        let table = self
            .config()
            .map(|s| s.config().gateway.platform(&routing.platform))
            .unwrap_or_default();
        let mut entries: Vec<String> = self
            .shared
            .agents
            .menu_commands(&self.profile)
            .into_iter()
            .filter(|c| auth::may_run(&table, routing.chat_type, &routing.sender_id, &c.name))
            .map(|c| {
                if c.description.is_empty() {
                    format!("/{}", c.name)
                } else {
                    format!("/{} - {}", c.name, c.description)
                }
            })
            .collect();
        entries.dedup();
        entries
    }

    /// Shows page `page` of the command list; `listing` is the key of the
    /// message a paging button edits.
    fn commands_page(&mut self, routing: &Routing, page: usize, listing: Option<String>) {
        let entries = self.available_commands(routing);
        let (shown, pages) = match page_of(&entries, page, per_page(&routing.platform)) {
            Ok(found) => found,
            Err(refusal) => {
                self.say(&refusal);
                return;
            }
        };
        let native = self.tier() == Interactive::Native;
        let mut text = format!("Commands you can use here:\n{}", shown.join("\n"));
        if !native && pages > 1 {
            let _ = write!(text, "\npage {page}/{pages}");
        }
        let id = listing
            .clone()
            .unwrap_or_else(|| format!("{COMMANDS}-{}", crate::tickets::random_hex(6)));
        let buttons = if native {
            self.paging_buttons(&id, page, pages)
        } else {
            Vec::new()
        };
        let key = format!("page:{id}");
        let edit_of = listing.and_then(|_| {
            self.shared
                .store(&self.profile)
                .and_then(|store| store.outbound_message_id(&key).ok().flatten())
        });
        let body = json!({"text": text, "buttons": buttons}).to_string();
        let entry_key = match &edit_of {
            Some(_) => format!("{key}:{page}:{}", crate::tickets::random_hex(4)),
            None => key,
        };
        self.deliver(entry_key, OutboundKind::Approval, body, edit_of);
    }

    fn paging_buttons(&self, id: &str, page: usize, pages: usize) -> Vec<Button> {
        let key = &self.shared.card_key;
        let mut buttons = Vec::new();
        if page > 1 {
            buttons.push(Button {
                label: "\u{2039} Previous".to_owned(),
                data: key.data(CardKind::Page, id, &(page - 1).to_string()),
            });
        }
        if page < pages {
            buttons.push(Button {
                label: "Next \u{203a}".to_owned(),
                data: key.data(CardKind::Page, id, &(page + 1).to_string()),
            });
        }
        buttons
    }

    /// A paging button was pressed (its signature verified).
    pub(super) fn page_pressed(&mut self, routing: &Routing, id: &str, choice: &str) {
        if id.starts_with(super::models::PICKER) {
            self.picker_pressed(routing, id, choice);
            return;
        }
        let Ok(page) = choice.parse::<usize>() else {
            return;
        };
        if id.starts_with(COMMANDS) {
            self.commands_page(routing, page, Some(id.to_owned()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thirty_seven_commands_make_two_pages_of_twenty() {
        let entries: Vec<String> = (0..37).map(|i| format!("/c{i}")).collect();
        let (first, pages) = page_of(&entries, 1, 20).unwrap();
        assert_eq!((first.len(), pages), (20, 2));
        let (second, _) = page_of(&entries, 2, 20).unwrap();
        assert_eq!(second.len(), 17);
        assert_eq!(
            page_of(&entries, 3, 20).unwrap_err(),
            "page 3 does not exist (2 pages)"
        );
        assert_eq!(page_of(&entries, 0, 20).unwrap_err(), no_such_page(0, 2));
    }

    #[test]
    fn telegram_pages_hold_fifteen() {
        assert_eq!(per_page("telegram"), 15);
        assert_eq!(per_page("discord"), 20);
        assert!(requested("x").is_err());
        assert_eq!(requested(" 2 ").unwrap(), 2);
    }
}
