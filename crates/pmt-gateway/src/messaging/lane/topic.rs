//! `/topic new|list|rename|restore` (spec: messaging-gateway, Multi-session
//! DM topics): managing the topics of a DM whose adapter declares
//! `dm_topics`. A new topic's session starts with its first message; a
//! restored topic continues an earlier session of the same chat; `/new`
//! inside a topic starts a new session in that topic (the ordinary `/new`).

use std::fmt::Write as _;

use pmt_store::Origin;

use super::Lane;
use crate::messaging::adapter::SendError;
use crate::messaging::key::Routing;
use crate::messaging::topics::DEFAULT_NAME;

const USAGE: &str = "Usage: /topic new <name> | list | rename <name> | restore <session-id>";

impl Lane {
    /// `/topic ...`.
    pub(super) async fn topic_command(&mut self, routing: &Routing, args: &str) {
        let args = args.trim();
        let (verb, rest) = args.split_once(' ').unwrap_or((args, ""));
        let rest = rest.trim();
        if !self.topics_enabled(routing) {
            self.say("Topics are not enabled in this chat.");
            return;
        }
        match verb {
            "new" => self.topic_new(routing, rest).await,
            "list" => self.topic_list(routing),
            "rename" => self.topic_rename(routing, rest).await,
            "restore" => self.topic_restore(routing, rest).await,
            _ => self.say(USAGE),
        }
    }

    fn topics_enabled(&self, routing: &Routing) -> bool {
        self.shared
            .adapters
            .descriptor(&self.profile, &routing.platform)
            .is_some_and(|d| {
                self.shared
                    .topics
                    .enabled(&d, &self.profile, &routing.platform, &routing.chat_id)
            })
    }

    /// Opens topic `title` in the chat; its thread id, or `None` after
    /// telling the chat why not (a refusal makes the chat single-session).
    async fn open_topic(&self, routing: &Routing, title: &str) -> Option<String> {
        let adapter = self
            .shared
            .adapters
            .adapter(&self.profile, &routing.platform)?;
        match adapter.open_thread(&routing.chat_id, title).await {
            Ok(thread) => Some(thread),
            Err(err) => {
                let why = err.describe();
                if matches!(err, SendError::Unsupported | SendError::Permanent(_)) {
                    self.shared.topics.refused(
                        &self.profile,
                        &routing.platform,
                        &routing.chat_id,
                        &why,
                    );
                    self.say(
                        "Topics are unavailable in this chat; it continues as one conversation.",
                    );
                } else {
                    self.say(&format!("The topic was not created: {why}"));
                }
                None
            }
        }
    }

    async fn topic_new(&mut self, routing: &Routing, name: &str) {
        let title = if name.is_empty() { DEFAULT_NAME } else { name };
        let Some(thread) = self.open_topic(routing, title).await else {
            return;
        };
        if name.is_empty() {
            self.shared.topics.opened_unnamed(
                &self.profile,
                &routing.platform,
                &routing.chat_id,
                &thread,
            );
        }
        let topic = Routing {
            thread_id: Some(thread),
            ..routing.clone()
        };
        let session = self.session.clone().unwrap_or_default();
        self.post_in(
            &topic,
            &session,
            &format!("Topic \"{title}\" is ready; write here."),
        );
        self.say(&format!("Topic \"{title}\" created."));
    }

    fn topic_list(&self, routing: &Routing) {
        let Some(store) = self.shared.store(&self.profile) else {
            return;
        };
        let routes = store
            .thread_routes(&routing.platform, &routing.chat_id)
            .unwrap_or_default();
        if routes.is_empty() {
            self.say("No topics with a session yet; /topic new <name> opens one.");
            return;
        }
        let mut text = String::from("Topics in this chat:");
        for route in routes {
            let label = store.session(route.session_id).map_or_else(
                |_| route.session_id.to_string(),
                |s| super::history::label(&s),
            );
            let thread = route.thread_id.unwrap_or_default();
            let _ = write!(text, "\n- topic {thread}: {label}");
        }
        self.say(&text);
    }

    async fn topic_rename(&mut self, routing: &Routing, name: &str) {
        let Some(thread) = routing.thread_id.clone() else {
            self.say("Send /topic rename <name> inside the topic to rename.");
            return;
        };
        if name.is_empty() {
            self.say(USAGE);
            return;
        }
        let Some(adapter) = self
            .shared
            .adapters
            .adapter(&self.profile, &routing.platform)
        else {
            return;
        };
        match adapter.rename_thread(&routing.chat_id, &thread, name).await {
            Ok(()) => {
                self.shared.topics.take_unnamed(
                    &self.profile,
                    &routing.platform,
                    &routing.chat_id,
                    &thread,
                );
                self.say(&format!("Topic renamed to \"{name}\"."));
            }
            Err(err) => self.say(&format!("The topic was not renamed: {}", err.describe())),
        }
    }

    /// `/topic restore <session-id>`: a new topic continuing an earlier
    /// session of this chat, titled with its title.
    async fn topic_restore(&mut self, routing: &Routing, wanted: &str) {
        let Some(store) = self.shared.store(&self.profile) else {
            return;
        };
        let found = wanted
            .parse()
            .ok()
            .and_then(|id| store.session(id).ok())
            .filter(|s| {
                matches!(&s.origin, Origin::Gateway { platform, chat, .. }
                    if *platform == routing.platform && *chat == routing.chat_id)
            });
        let Some(session) = found else {
            self.say(&format!("No session {wanted} of this chat to restore."));
            return;
        };
        let title = session
            .title
            .clone()
            .unwrap_or_else(|| format!("Restored {}", session.id));
        let Some(thread) = self.open_topic(routing, &title).await else {
            return;
        };
        let topic = Routing {
            thread_id: Some(thread),
            ..routing.clone()
        };
        if session.ended_at.is_some()
            && let Err(err) = store.reopen_session(session.id)
        {
            self.say(err.message());
            return;
        }
        let id = session.id.to_string();
        if let Err(err) = self.bind_elsewhere(&topic, &id) {
            self.say(err.message());
            return;
        }
        self.post_in(
            &topic,
            &id,
            &format!("Restored \"{title}\"; continue here."),
        );
        self.say(&format!("Topic \"{title}\" continues session {id}."));
    }

    /// After a topic session's first turn: a topic still carrying its
    /// default name takes the session's title.
    pub(super) async fn name_default_topic(&self, title: &str) {
        let Some(thread) = self.routing.thread_id.clone() else {
            return;
        };
        let routing = &self.routing;
        if !self.shared.topics.take_unnamed(
            &self.profile,
            &routing.platform,
            &routing.chat_id,
            &thread,
        ) {
            return;
        }
        if let Some(adapter) = self
            .shared
            .adapters
            .adapter(&self.profile, &routing.platform)
            && let Err(err) = adapter
                .rename_thread(&routing.chat_id, &thread, title)
                .await
        {
            tracing::debug!("topic not renamed: {}", err.describe());
        }
    }
}
