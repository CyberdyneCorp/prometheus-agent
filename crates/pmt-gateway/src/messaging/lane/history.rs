//! The chat's earlier sessions (spec: messaging-gateway, Interactive model
//! picker and chat model override): `/sessions [page]` lists the sessions
//! started in this chat (and thread), newest first, 10 per page and
//! numbered across pages; `/resume <n>` (or a session id of this chat)
//! routes the conversation back to one, reopening it when it had ended.
//! The session it leaves stays as it is.

use std::fmt::Write as _;

use pmt_core::id::SessionId;
use pmt_store::gateway::routes::NewRoute;
use pmt_store::{ListOptions, Origin, Session, Source};

use super::Lane;
use crate::messaging::key::Routing;

/// Sessions on one page of `/sessions`.
const PER_PAGE: usize = 10;

/// What a session is called in a listing.
pub(crate) fn label(session: &Session) -> String {
    let title = session.title.as_deref().unwrap_or("untitled");
    let at = session.started_at.strftime("%Y-%m-%d %H:%M");
    format!("{title} ({at}, {})", session.id)
}

impl Lane {
    /// The sessions started in `routing`'s chat and thread, newest first.
    pub(super) fn chat_sessions(&self, routing: &Routing) -> Vec<Session> {
        let Some(store) = self.shared.store(&self.profile) else {
            return Vec::new();
        };
        let options = ListOptions {
            all: true,
            sources: vec![Source::Gateway],
            ..ListOptions::default()
        };
        store
            .list_sessions(&options)
            .unwrap_or_default()
            .into_iter()
            .filter(|s| match &s.origin {
                Origin::Gateway {
                    platform,
                    chat,
                    thread,
                    ..
                } => {
                    *platform == routing.platform
                        && *chat == routing.chat_id
                        && *thread == routing.thread_id
                }
                _ => false,
            })
            .collect()
    }

    /// `/sessions [page]`.
    pub(super) fn sessions_command(&mut self, routing: &Routing, args: &str) {
        let wanted = match args.trim() {
            "" => 1,
            other => match other.parse::<usize>() {
                Ok(n) if n > 0 => n,
                _ => {
                    self.say("Usage: /sessions [page]");
                    return;
                }
            },
        };
        let sessions = self.chat_sessions(routing);
        let entries: Vec<String> = sessions.iter().map(label).collect();
        let (shown, pages) = match super::pages::page_of(&entries, wanted, PER_PAGE) {
            Ok(found) => found,
            Err(refusal) => {
                self.say(&refusal);
                return;
            }
        };
        let current = self.current_session();
        let mut text = String::from("Sessions in this chat (/resume <n> switches):");
        for (i, entry) in shown.iter().enumerate() {
            let n = (wanted - 1) * PER_PAGE + i + 1;
            let here = sessions
                .get(n - 1)
                .is_some_and(|s| Some(s.id.to_string()) == current);
            let mark = if here { "  (current)" } else { "" };
            let _ = write!(text, "\n{n}. {entry}{mark}");
        }
        if pages > 1 {
            let _ = write!(text, "\npage {wanted}/{pages}");
        }
        self.say(&text);
    }

    /// `/resume <n | session id>`: this chat's session `n` of `/sessions`
    /// becomes the conversation's session.
    pub(super) fn resume_command(&mut self, routing: &Routing, args: &str) {
        if self.running.is_some() {
            self.say(&super::commands::rejected("resume"));
            return;
        }
        let sessions = self.chat_sessions(routing);
        let wanted = args.trim();
        let found = match wanted.parse::<usize>() {
            Ok(n) => n.checked_sub(1).and_then(|i| sessions.get(i)),
            Err(_) => sessions.iter().find(|s| s.id.to_string() == wanted),
        };
        let Some(session) = found else {
            self.say("Usage: /resume <n> (a number from /sessions)");
            return;
        };
        let (id, text) = (session.id, label(session));
        match self.route_to(routing, id) {
            Ok(()) => {
                self.say(&format!("Resumed {text}."));
                self.say_memory_summary(&id.to_string());
            }
            Err(err) => self.say(err.message()),
        }
    }

    /// `Memory: +a ~p -r (review with /memory review)` for a session that
    /// ended or was resumed here, when the agent changed memory in it
    /// (spec: memory, Reviewable memory changes per session).
    pub(super) fn say_memory_summary(&self, session: &str) {
        match self.shared.agents.memory_summary(&self.profile, session) {
            Ok(Some(line)) => self.say(&line),
            Ok(None) => {}
            Err(err) => tracing::warn!("memory review summary skipped: {err}"),
        }
    }

    /// Routes the conversation to the existing session `id` (reopened
    /// when it had ended).
    pub(super) fn route_to(
        &mut self,
        routing: &Routing,
        id: SessionId,
    ) -> Result<(), pmt_core::error::Error> {
        let store = self.shared.store(&self.profile).ok_or_else(|| {
            pmt_core::error::Error::new(
                &pmt_core::error::codes::PROFILE_NOT_FOUND,
                "the profile is not served",
            )
        })?;
        if store.session(id)?.ended_at.is_some() {
            store.reopen_session(id)?;
        }
        store.bind_route(
            &NewRoute {
                key: self.key.as_str().to_owned(),
                session_id: id,
                platform: routing.platform.clone(),
                chat_id: routing.chat_id.clone(),
                thread_id: routing.thread_id.clone(),
            },
            None,
        )?;
        self.adopt(&id.to_string());
        self.horizon = self.status.seq.load(std::sync::atomic::Ordering::Acquire);
        Ok(())
    }
}
