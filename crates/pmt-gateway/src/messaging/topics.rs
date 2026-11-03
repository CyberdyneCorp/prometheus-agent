//! Multi-session DM topics (spec: messaging-gateway, Multi-session DM
//! topics). Where the adapter declares `dm_topics` (Telegram private chats
//! in topic mode, with `options.dm_topics.enabled`), each topic of a DM is
//! its own conversation (its thread id is in the key) and the root DM is a
//! lobby: it takes commands only and answers anything else with
//! [`LOBBY`]. A chat where the platform refuses to open topics falls back
//! to an ordinary single-session DM.

use std::collections::HashSet;
use std::sync::Mutex;

use super::adapter::Inbound;
use super::descriptor::Descriptor;
use super::key::ChatType;

/// What the lobby answers to anything but a command.
pub(crate) const LOBBY: &str = "Open or create a topic to chat (/topic new <name>)";

/// The name of a topic opened without one (renamed to the session's title
/// once it has one).
pub(crate) const DEFAULT_NAME: &str = "New chat";

/// Topic state the gateway keeps across conversations.
#[derive(Debug, Default)]
pub(crate) struct TopicState {
    /// Chats where the platform refused to open a topic: single-session.
    unavailable: Mutex<HashSet<String>>,
    /// Topics still carrying [`DEFAULT_NAME`].
    default_named: Mutex<HashSet<String>>,
}

fn chat_key(profile: &str, platform: &str, chat: &str) -> String {
    format!("{profile}\u{1f}{platform}\u{1f}{chat}")
}

fn topic_key(profile: &str, platform: &str, chat: &str, thread: &str) -> String {
    format!("{profile}\u{1f}{platform}\u{1f}{chat}\u{1f}{thread}")
}

impl TopicState {
    /// Whether `chat` of the adapter has topics (declared and not refused).
    pub(crate) fn enabled(
        &self,
        descriptor: &Descriptor,
        profile: &str,
        platform: &str,
        chat: &str,
    ) -> bool {
        descriptor.dm_topics
            && !self
                .unavailable
                .lock()
                .expect("topics poisoned")
                .contains(&chat_key(profile, platform, chat))
    }

    /// The platform refused to open a topic in `chat`: logged once, and
    /// the chat is an ordinary DM from now on.
    pub(crate) fn refused(&self, profile: &str, platform: &str, chat: &str, why: &str) {
        let first = self
            .unavailable
            .lock()
            .expect("topics poisoned")
            .insert(chat_key(profile, platform, chat));
        if first {
            tracing::warn!(
                profile,
                platform,
                "topics are unavailable in a DM ({why}); it stays a single-session chat"
            );
        }
    }

    /// Remembers a topic opened with [`DEFAULT_NAME`].
    pub(crate) fn opened_unnamed(&self, profile: &str, platform: &str, chat: &str, thread: &str) {
        self.default_named
            .lock()
            .expect("topics poisoned")
            .insert(topic_key(profile, platform, chat, thread));
    }

    /// Whether the topic still has its default name; forgets it (it is
    /// about to be renamed, or was renamed by hand).
    pub(crate) fn take_unnamed(
        &self,
        profile: &str,
        platform: &str,
        chat: &str,
        thread: &str,
    ) -> bool {
        self.default_named
            .lock()
            .expect("topics poisoned")
            .remove(&topic_key(profile, platform, chat, thread))
    }
}

/// Whether `inbound` lands in a topic lobby: a DM's root, of an adapter
/// with topics, carrying something other than a command (or a pressed
/// button).
pub(crate) fn in_lobby(enabled: bool, inbound: &Inbound) -> bool {
    enabled
        && inbound.chat_type == ChatType::Dm
        && inbound.thread_id.is_none()
        && inbound.callback.is_none()
        && !inbound.text.trim_start().starts_with('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_root_dm_text_lands_in_the_lobby() {
        let text = Inbound::text(ChatType::Dm, "1", "1", "hello");
        assert!(in_lobby(true, &text));
        assert!(!in_lobby(false, &text), "topics off");
        let command = Inbound::text(ChatType::Dm, "1", "1", "/topic new x");
        assert!(!in_lobby(true, &command));
        let mut topic = text.clone();
        topic.thread_id = Some("7".into());
        assert!(!in_lobby(true, &topic));
        let group = Inbound::text(ChatType::Group, "1", "1", "hello");
        assert!(!in_lobby(true, &group));
    }

    #[test]
    fn a_refused_chat_falls_back_and_default_names_are_taken_once() {
        let state = TopicState::default();
        let descriptor = Descriptor {
            dm_topics: true,
            ..Descriptor::new("telegram", 4096)
        };
        assert!(state.enabled(&descriptor, "p", "telegram", "1"));
        state.refused("p", "telegram", "1", "not a forum");
        assert!(!state.enabled(&descriptor, "p", "telegram", "1"));
        assert!(state.enabled(&descriptor, "p", "telegram", "2"));
        state.opened_unnamed("p", "telegram", "2", "9");
        assert!(state.take_unnamed("p", "telegram", "2", "9"));
        assert!(!state.take_unnamed("p", "telegram", "2", "9"));
    }
}
