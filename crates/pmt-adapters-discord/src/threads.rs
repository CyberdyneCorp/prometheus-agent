//! Threads and forum channels (spec: platform-adapters, Discord
//! auto-threading and forum channels). A message in a thread is the
//! thread of its parent channel. With `options.auto_thread` (default on),
//! a mention in a text channel opens a thread named after the message
//! (mentions removed, at most 80 characters) and the conversation goes on
//! there without further mentions; free-response channels get threads
//! only with `free_response_auto_thread`, `no_thread_channels` never, and
//! replies never. The thread is renamed to the session title after the
//! first turn while it still has its first name. In a forum channel every
//! agent-initiated send is a new post titled by its first line.

use std::collections::HashMap;
use std::sync::Mutex;

use pmt_config::platforms::DiscordOptions;
use pmt_gateway::messaging::{ChatType, Inbound, SendError};
use reqwest::Method;
use serde_json::{Value, json};

use crate::api::{Api, Body};

/// Channel types (Discord's numbering).
const GUILD_TEXT: u64 = 0;
const GUILD_ANNOUNCEMENT: u64 = 5;
const THREADS: [u64; 3] = [10, 11, 12];
/// A forum, and a media channel (a forum of media posts).
const FORUMS: [u64; 2] = [15, 16];

/// What the adapter knows of a channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Channel {
    pub(crate) kind: u64,
    pub(crate) parent: Option<String>,
    pub(crate) name: String,
}

impl Channel {
    pub(crate) fn is_thread(&self) -> bool {
        THREADS.contains(&self.kind)
    }

    pub(crate) fn is_forum(&self) -> bool {
        FORUMS.contains(&self.kind)
    }
}

/// The channels seen, and the threads the bot opened (with their first
/// names, for the rename after the first turn).
#[derive(Debug, Default)]
pub(crate) struct Channels {
    known: Mutex<HashMap<String, Channel>>,
    opened: Mutex<HashMap<String, String>>,
}

impl Channels {
    /// Remembers a channel object (`CHANNEL_CREATE`, `THREAD_CREATE`, a
    /// guild's channel lists, a REST answer).
    pub(crate) fn learn(&self, value: &Value) {
        let Some(id) = value["id"].as_str() else {
            return;
        };
        let channel = Channel {
            kind: value["type"].as_u64().unwrap_or(GUILD_TEXT),
            parent: value["parent_id"].as_str().map(str::to_owned),
            name: value["name"].as_str().unwrap_or_default().to_owned(),
        };
        self.known
            .lock()
            .expect("channels poisoned")
            .insert(id.to_owned(), channel);
    }

    /// Remembers every channel and thread of a `GUILD_CREATE`.
    pub(crate) fn learn_guild(&self, guild: &Value) {
        for key in ["channels", "threads"] {
            for channel in guild[key].as_array().into_iter().flatten() {
                self.learn(channel);
            }
        }
    }

    /// The channel `id`, asked of Discord when unknown.
    pub(crate) async fn get(&self, api: &Api, id: &str) -> Option<Channel> {
        if let Some(known) = self.known.lock().expect("channels poisoned").get(id) {
            return Some(known.clone());
        }
        let found = api
            .call(Method::GET, &format!("/channels/{id}"), Body::Empty)
            .await
            .ok()?;
        self.learn(&found);
        self.known
            .lock()
            .expect("channels poisoned")
            .get(id)
            .cloned()
    }

    fn opened_by_bot(&self, thread: &str) -> bool {
        self.opened
            .lock()
            .expect("channels poisoned")
            .contains_key(thread)
    }

    /// The first name of a thread the bot opened and has not renamed yet.
    pub(crate) fn take_first_name(&self, thread: &str) -> Option<String> {
        self.opened
            .lock()
            .expect("channels poisoned")
            .remove(thread)
    }
}

/// What routing a server message needs.
pub(crate) struct Routed<'a> {
    pub(crate) api: &'a Api,
    pub(crate) channels: &'a Channels,
    pub(crate) options: &'a DiscordOptions,
}

/// The message text without mention tokens, at most 80 characters: a
/// new thread's name.
pub(crate) fn thread_name(text: &str) -> String {
    let words: Vec<&str> = text
        .split_whitespace()
        .filter(|w| !(w.starts_with("<@") && w.ends_with('>')))
        .collect();
    let name: String = words.join(" ").chars().take(80).collect();
    if name.is_empty() {
        "Conversation".to_owned()
    } else {
        name
    }
}

/// The title of a forum post: the first non-empty line, at most 100
/// characters.
pub(crate) fn post_title(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("Message");
    line.chars().take(100).collect()
}

impl Routed<'_> {
    /// Places `event` (a server message, `mentioned` or not) in its
    /// channel or thread; `None` when it is not for the bot.
    pub(crate) async fn route(
        &self,
        mut event: Inbound,
        mentioned: bool,
        raw: &Value,
    ) -> Option<Inbound> {
        if event.chat_type == ChatType::Dm {
            return Some(event);
        }
        let channel = self.channels.get(self.api, &event.chat_id).await;
        if let Some(thread) = channel.as_ref().filter(|c| c.is_thread()) {
            let parent = thread
                .parent
                .clone()
                .unwrap_or_else(|| event.chat_id.clone());
            let ours = self.channels.opened_by_bot(&event.chat_id)
                || self
                    .channels
                    .get(self.api, &parent)
                    .await
                    .is_some_and(|p| p.is_forum());
            if !mentioned && !ours && self.options.require_mention {
                return None;
            }
            event.addressing.in_bot_thread = ours;
            event.thread_id = Some(std::mem::replace(&mut event.chat_id, parent));
            return Some(event);
        }
        let free = self.options.free_response_channels.contains(&event.chat_id);
        if self.options.require_mention && !mentioned && !free {
            return None;
        }
        let text_channel = channel
            .as_ref()
            .is_none_or(|c| matches!(c.kind, GUILD_TEXT | GUILD_ANNOUNCEMENT));
        let reply = raw["message_reference"].is_object() || raw["type"].as_u64() == Some(19);
        let threads = self.options.auto_thread
            && text_channel
            && !reply
            && !self.options.no_thread_channels.contains(&event.chat_id)
            && if free {
                self.options.free_response_auto_thread
            } else {
                mentioned
            };
        if threads {
            match self.open(&event).await {
                Ok(thread) => event.thread_id = Some(thread),
                Err(err) => tracing::warn!(
                    platform = "discord",
                    "no thread opened, answering inline: {}",
                    err.describe()
                ),
            }
        }
        Some(event)
    }

    /// Opens a thread on the message of `event`.
    async fn open(&self, event: &Inbound) -> Result<String, SendError> {
        let name = thread_name(&event.text);
        let path = format!(
            "/channels/{}/messages/{}/threads",
            event.chat_id, event.message_id
        );
        let body = Body::Json(json!({"name": name, "auto_archive_duration": 1440}));
        let created = self.api.call(Method::POST, &path, body).await?;
        let id = created["id"]
            .as_str()
            .ok_or_else(|| SendError::Server("the thread has no id".to_owned()))?
            .to_owned();
        self.channels.learn(&created);
        self.channels
            .opened
            .lock()
            .expect("channels poisoned")
            .insert(id.clone(), name);
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_names_drop_mentions_and_stay_short() {
        assert_eq!(
            thread_name("<@123> summarize the incident"),
            "summarize the incident"
        );
        assert_eq!(thread_name("<@!123>"), "Conversation");
        assert_eq!(thread_name(&"x ".repeat(60)).chars().count(), 80);
        assert_eq!(post_title("\n  Weekly report  \nbody"), "Weekly report");
    }
}
