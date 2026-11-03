//! Topics (spec: platform-adapters, Telegram topics and per-topic skills):
//! each private-chat topic (threaded-mode DM) and each forum topic is its
//! own thread. Declared DM topics without a `thread_id` are created at
//! connect; their ids, and topics created elsewhere, are remembered in
//! `<profile>/platforms/telegram/topics.json` (never by rewriting
//! `config.toml`). A topic's `skill` is preloaded into its new sessions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;

use pmt_config::platforms::{TelegramOptions, TopicSpec};
use pmt_gateway::messaging::SendError;
use serde_json::json;

use crate::api::Api;

/// Thread ids by chat and topic name.
type Recorded = BTreeMap<String, BTreeMap<String, i64>>;

/// The declared and remembered topics of one bot.
#[derive(Debug)]
pub(crate) struct Topics {
    file: PathBuf,
    dm: BTreeMap<String, Vec<TopicSpec>>,
    group: BTreeMap<String, Vec<TopicSpec>>,
    recorded: Mutex<Recorded>,
    /// Chats already logged as not being forums.
    not_forum: Mutex<BTreeSet<String>>,
}

impl Topics {
    /// The topics of `options`, with the ids remembered in `file`.
    pub(crate) fn load(file: PathBuf, options: &TelegramOptions) -> Self {
        let recorded = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            file,
            dm: options.dm_topics.chats.clone(),
            group: options.group_topics.clone(),
            recorded: Mutex::new(recorded),
            not_forum: Mutex::default(),
        }
    }

    fn thread_of(&self, chat: &str, spec: &TopicSpec) -> Option<i64> {
        spec.thread_id.or_else(|| {
            self.recorded
                .lock()
                .expect("topics poisoned")
                .get(chat)
                .and_then(|topics| topics.get(&spec.name).copied())
        })
    }

    fn persist(&self, recorded: &Recorded) {
        let written = self
            .file
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                let text = serde_json::to_string_pretty(recorded).unwrap_or_default();
                std::fs::write(&self.file, text)
            });
        if let Err(err) = written {
            tracing::warn!(
                platform = "telegram",
                "topic ids not saved to {}: {err}",
                self.file.display()
            );
        }
    }

    /// Remembers that `chat`'s topic `name` is thread `thread`.
    pub(crate) fn record(&self, chat: &str, name: &str, thread: i64) {
        let mut recorded = self.recorded.lock().expect("topics poisoned");
        let known = recorded.get(chat).and_then(|t| t.get(name)) == Some(&thread);
        if !known {
            recorded
                .entry(chat.to_owned())
                .or_default()
                .insert(name.to_owned(), thread);
            self.persist(&recorded);
        }
    }

    /// Creates the declared DM topics that have no thread yet.
    pub(crate) async fn create_missing(&self, api: &Api) {
        for (chat, specs) in &self.dm {
            for spec in specs {
                if self.thread_of(chat, spec).is_some() {
                    continue;
                }
                match create(api, chat, spec).await {
                    Ok(thread) => self.record(chat, &spec.name, thread),
                    Err(SendError::Permanent(why)) if why.contains("not a forum") => {
                        if self
                            .not_forum
                            .lock()
                            .expect("topics poisoned")
                            .insert(chat.clone())
                        {
                            tracing::warn!(
                                platform = "telegram",
                                chat,
                                "chat is not a forum; its topics are not created"
                            );
                        }
                        break;
                    }
                    Err(err) => tracing::warn!(
                        platform = "telegram",
                        chat,
                        topic = spec.name,
                        "topic not created: {}",
                        err.describe()
                    ),
                }
            }
        }
    }

    /// The skill preloaded into new sessions of `chat`'s thread.
    pub(crate) fn skill(&self, chat: &str, thread: &str) -> Option<String> {
        let thread: i64 = thread.parse().ok()?;
        [&self.dm, &self.group]
            .into_iter()
            .filter_map(|declared| declared.get(chat))
            .flatten()
            .find(|spec| self.thread_of(chat, spec) == Some(thread))
            .and_then(|spec| spec.skill.clone())
    }
}

/// Opens topic `name` in `chat` (a forum group, or a private chat with
/// topics); its thread id.
pub(crate) async fn open(api: &Api, chat: &str, name: &str) -> Result<String, SendError> {
    let spec = TopicSpec {
        name: name.chars().take(128).collect(),
        thread_id: None,
        icon_color: None,
        skill: None,
    };
    create(api, chat, &spec).await.map(|t| t.to_string())
}

/// Renames topic `thread` of `chat`.
pub(crate) async fn rename(
    api: &Api,
    chat: &str,
    thread: &str,
    name: &str,
) -> Result<(), SendError> {
    let thread: i64 = thread
        .parse()
        .map_err(|_| SendError::Permanent(format!("'{thread}' is not a topic id")))?;
    let name: String = name.chars().take(128).collect();
    let body = json!({"chat_id": crate::chat(chat), "message_thread_id": thread, "name": name});
    api.call("editForumTopic", &body).await.map(drop)
}

async fn create(api: &Api, chat: &str, spec: &TopicSpec) -> Result<i64, SendError> {
    let mut body = json!({"chat_id": crate::chat(chat), "name": spec.name});
    if let Some(color) = spec.icon_color {
        body["icon_color"] = json!(color);
    }
    let created = api.call("createForumTopic", &body).await?;
    created["message_thread_id"]
        .as_i64()
        .ok_or_else(|| SendError::Server("createForumTopic returned no thread id".to_owned()))
}
