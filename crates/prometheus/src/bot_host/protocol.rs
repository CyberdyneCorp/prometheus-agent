//! The bot protocol of canonical Bot Chats and the notices a turn carries
//! (spec: bot-mode, Bot protocol in canonical Bot Chats; Mentions from any
//! chat).
//!
//! The protocol section (the bot's own handle and its teammates) is put
//! into a Bot Chat's system prompt once, when the session's prompt is
//! built, and the roster it listed is remembered. A later roster change
//! reaches the chat as one `roster.update` notice stored with the next
//! turn's user message, never by rebuilding the prompt. An `@handle` in
//! any chat that names a bot of the live roster gets a notice naming
//! that bot; the text itself is never forwarded to it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use pmt_agent::turn::TurnNotes;
use pmt_config::ProfileScope;
use pmt_store::Store;

use super::roster::{self, Resolved, Teammate};

/// The store preference remembering the roster a Bot Chat's prompt (or
/// its last notice) listed.
fn seen_pref(session: &str) -> String {
    format!("bot.roster_seen.{session}")
}

/// One roster line: handle -> "title — role (machine)".
type Lines = BTreeMap<String, String>;

/// The teammates `me` sees: the other enabled bots of the home and the
/// registered peer gateways.
fn lines(scope: &ProfileScope, me: &str) -> Lines {
    let mut lines = Lines::new();
    for mate in roster::local(scope.home()).unwrap_or_default() {
        if mate.profile != me && mate.enabled {
            lines.insert(mate.handle(), line(&mate));
        }
    }
    for peer in scope.config().peers.keys() {
        lines.insert(
            format!("{peer}/<agent>"),
            format!("the agents of peer gateway {peer} (machine: {peer})"),
        );
    }
    lines
}

fn line(mate: &Teammate) -> String {
    let role = mate
        .description
        .as_deref()
        .map(|d| format!(" — {d}"))
        .unwrap_or_default();
    format!("{}{role} (machine: local)", mate.title)
}

/// The protocol section of `scope`'s canonical Bot Chat `session`, and
/// the roster it lists remembered for later notices.
#[must_use]
pub fn section(scope: &ProfileScope, store: &Store, session: &str) -> String {
    let me = roster::of(scope, store);
    let listed = lines(scope, &me.profile);
    remember(store, session, &listed);
    let mut text = format!(
        "# Bot protocol\nYou are the bot {} (\"{}\") in Bot Mode. Your teammates:\n",
        me.handle(),
        me.title
    );
    if listed.is_empty() {
        text.push_str("- (none yet)\n");
    }
    for (handle, about) in &listed {
        let _ = writeln!(text, "- {handle}: {about}");
    }
    text.push_str(
        "Message a teammate with the `message_agent` tool (target: its handle without @, a \
         title tag, <peer>/<agent>, or <handle>@<connection>). It returns at once; the reply, \
         or why it failed as [reason: <code>], arrives later in this chat. A message from a \
         teammate starts with \"Message from 🤖 <title> (@<handle>)\": answer it in your reply \
         (it is sent back), or answer [SILENT] to send nothing. Roster changes arrive as \
         roster.update notices.",
    );
    text
}

fn remember(store: &Store, session: &str, lines: &Lines) {
    if let Ok(text) = serde_json::to_string(lines)
        && let Err(err) = store.set_pref(&seen_pref(session), &text)
    {
        tracing::warn!("bot roster not remembered: {}", err.message());
    }
}

/// The `roster.update` notice for `session` when its roster changed since
/// it was last listed (and remembers the new one).
fn roster_update(scope: &ProfileScope, store: &Store, session: &str) -> Option<String> {
    let seen: Lines = store
        .pref(&seen_pref(session))
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str(&text).ok())?;
    let me = scope.name().to_string();
    let now = lines(scope, &me);
    if now == seen {
        return None;
    }
    let mut changes = Vec::new();
    for (handle, about) in &now {
        match seen.get(handle) {
            None => changes.push(format!("added {handle}: {about}")),
            Some(was) if was != about => changes.push(format!("changed {handle}: {about}")),
            Some(_) => {}
        }
    }
    for handle in seen.keys().filter(|h| !now.contains_key(*h)) {
        changes.push(format!("removed {handle}"));
    }
    remember(store, session, &now);
    Some(format!("[roster.update] {}", changes.join("; ")))
}

/// The `@words` of `text` that may name a bot: not part of an e-mail
/// address, outside nothing else (a mention is a word starting with `@`).
fn mentions(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let bytes = text.as_bytes();
    for (at, _) in text.match_indices('@') {
        let before = at.checked_sub(1).map(|i| bytes[i]);
        if before.is_some_and(|b| b.is_ascii_alphanumeric() || b"._%+-@".contains(&b)) {
            continue;
        }
        let rest = &text[at + 1..];
        let end = rest
            .find(|c: char| !(c.is_alphanumeric() || "-_/".contains(c)))
            .unwrap_or(rest.len());
        let word = rest[..end].trim_end_matches(['-', '/']);
        let email_like = rest[end..].starts_with('.')
            && rest[end + 1..].starts_with(|c: char| c.is_alphanumeric());
        if !word.is_empty() && !email_like && !found.contains(&word) {
            found.push(word);
        }
    }
    found
}

/// The notice naming each bot `text` mentions.
fn mention_notice(scope: &ProfileScope, text: &str) -> Option<String> {
    let words = mentions(text);
    if words.is_empty() {
        return None;
    }
    let teammates = roster::local(scope.home()).ok()?;
    let peers: Vec<String> = scope.config().peers.keys().cloned().collect();
    let named: Vec<String> = words
        .iter()
        .filter_map(
            |word| match roster::resolve(word, &teammates, &peers).ok()? {
                Resolved::Local(mate) if mate.enabled => Some(format!(
                    "@{word} is the bot profile `{}` (\"{}\") on this machine",
                    mate.profile, mate.title
                )),
                Resolved::Peer { peer, agent } => Some(format!(
                    "@{word} is agent `{agent}` on peer gateway `{peer}`"
                )),
                _ => None,
            },
        )
        .collect();
    (!named.is_empty()).then(|| {
        format!(
            "[mention] {}. Nothing was sent to them: contact them with message_agent if needed.",
            named.join("; ")
        )
    })
}

/// The notices of one session's turns.
pub struct Notes {
    scope: ProfileScope,
    store: Arc<Store>,
    bot_chat: bool,
}

/// The turn notices of a session of `scope` (`bot_chat`: its canonical
/// Bot Chat with the protocol on).
#[must_use]
pub fn notes(scope: &ProfileScope, store: &Arc<Store>, bot_chat: bool) -> Arc<dyn TurnNotes> {
    Arc::new(Notes {
        scope: scope.clone(),
        store: Arc::clone(store),
        bot_chat,
    })
}

impl TurnNotes for Notes {
    fn before_turn(&self, session_id: &str, user_text: &str) -> Vec<String> {
        let mut notices = Vec::new();
        if self.bot_chat
            && let Some(update) = roster_update(&self.scope, &self.store, session_id)
        {
            notices.push(update);
        }
        if let Some(mentioned) = mention_notice(&self.scope, user_text) {
            notices.push(mentioned);
        }
        notices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_skip_email_addresses_and_keep_handles() {
        assert_eq!(
            mentions("@researcher have a look, and mail bob@example.com or @writer."),
            ["researcher", "writer"]
        );
        assert_eq!(mentions("ping @homelab/research now"), ["homelab/research"]);
        assert!(mentions("write to @example.com").is_empty());
        assert!(mentions("no mention here").is_empty());
    }
}
