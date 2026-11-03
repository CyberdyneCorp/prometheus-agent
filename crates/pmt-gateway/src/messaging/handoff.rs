//! Session handoff to a messaging platform (spec: messaging-gateway,
//! Session handoff to a messaging platform). A terminal session
//! (`/handoff <platform>[:<chat_id>[:<thread_id>]]` in the CLI or TUI)
//! moves to a chat of the same profile: the platform's home channel when
//! no chat is named, in a new thread or topic where the adapter opens
//! them. The chat must be one the profile may reach (its home channel, an
//! allowed chat, or a chat it already talks in). The destination gets a
//! role-attributed replay of the last 20 messages, the session a notice
//! that it continues there, and it is parked for that chat: the next
//! message written there continues it.

use std::fmt::Write as _;

use pmt_core::error::{Error, codes};
use pmt_store::gateway::outbox::OutboundKind;
use pmt_store::{Role, Visibility};

use super::Gateway;
use super::adapter::SendError;
use super::target::Resolved;

/// Messages the destination is shown.
const REPLAY: usize = 20;

/// Where a handoff went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandedOff {
    /// The platform.
    pub platform: String,
    /// `platform:chat_id[:thread_id]`.
    pub destination: String,
}

/// The replay of `messages` (role and text; user and assistant text
/// only), oldest first.
fn replay(session: &str, messages: &[(Role, &str)]) -> String {
    let shown: Vec<&(Role, &str)> = messages
        .iter()
        .filter(|(role, text)| {
            matches!(role, Role::User | Role::Assistant) && !text.trim().is_empty()
        })
        .collect();
    let start = shown.len().saturating_sub(REPLAY);
    let mut text = format!("Continuing session {session} here. The conversation so far:");
    for (role, content) in &shown[start..] {
        let who = if *role == Role::User {
            "You"
        } else {
            "Assistant"
        };
        let _ = write!(text, "\n\n{who}: {}", content.trim());
    }
    text
}

impl Gateway {
    /// Hands `session` of `profile` over to `target`.
    ///
    /// # Errors
    ///
    /// `session.busy` while a turn of it runs; the target's resolution
    /// errors; `gateway.target_refused` for a chat the profile may not
    /// reach; store errors.
    pub async fn handoff(
        &self,
        profile: &str,
        session: &str,
        target: &str,
    ) -> Result<HandedOff, Error> {
        let shared = &self.shared;
        if shared
            .lane_of_session(session)
            .is_some_and(|lane| lane.running())
        {
            return Err(Error::new(
                &codes::SESSION_BUSY,
                "a turn is running; wait for it or stop it before /handoff",
            ));
        }
        let resolved = self.resolve_target(profile, target).await?;
        self.authorized(profile, &resolved)?;
        let store = shared.profile_store(profile)?;
        let id: pmt_core::id::SessionId = session.parse()?;
        let found = store.session(id)?;
        let destination = self
            .handoff_thread(profile, resolved, found.title.as_deref())
            .await?;
        store.park_handoff(
            &destination.platform,
            &destination.chat_id,
            destination.thread_id.as_deref(),
            id,
        )?;
        let messages = store.messages(id, Visibility::Active)?;
        let lines: Vec<(Role, &str)> = messages
            .iter()
            .map(|m| (m.role, m.content.as_deref().unwrap_or_default()))
            .collect();
        self.enqueue_as(
            profile,
            &destination,
            &replay(session, &lines),
            &[],
            OutboundKind::Notification,
        )?;
        let label = destination.label();
        let notice = format!(
            "[The conversation continues on {} ({label}); the user's next messages arrive from there.]",
            destination.platform
        );
        shared
            .agents
            .append_notice(profile, session, "handoff", &notice)
            .await?;
        tracing::info!(profile, session, destination = label, "session handed off");
        Ok(HandedOff {
            platform: destination.platform,
            destination: label,
        })
    }

    /// Refuses a destination outside the profile's home channels, allowed
    /// chats and chats it already talks in.
    fn authorized(&self, profile: &str, to: &Resolved) -> Result<(), Error> {
        let known = self.known_targets(profile, Some(&to.platform))?;
        let chat = format!("{}:{}", to.platform, to.chat_id);
        let reachable = known
            .iter()
            .any(|t| t.target == chat || t.target.starts_with(&format!("{chat}:")));
        if reachable {
            Ok(())
        } else {
            Err(Error::new(
                &codes::GATEWAY_TARGET_REFUSED,
                format!(
                    "{} is not an authorized chat of profile '{profile}'",
                    to.label()
                ),
            ))
        }
    }

    /// A new thread (or topic) for the session where the adapter opens
    /// them and no thread was named; the chat itself otherwise.
    async fn handoff_thread(
        &self,
        profile: &str,
        to: Resolved,
        title: Option<&str>,
    ) -> Result<Resolved, Error> {
        if to.thread_id.is_some() {
            return Ok(to);
        }
        let Some(slot) = self.shared.adapters.ready(profile, &to.platform) else {
            return Ok(to);
        };
        if !slot.adapter.describe().supports_threads {
            return Ok(to);
        }
        let title = title.unwrap_or("Continued from the terminal");
        match slot.adapter.open_thread(&to.chat_id, title).await {
            Ok(thread) => Ok(Resolved {
                thread_id: Some(thread),
                ..to
            }),
            Err(SendError::Unsupported) => Ok(to),
            Err(err) => Err(Error::new(
                &codes::GATEWAY_TARGET_REFUSED,
                format!("no thread for the handoff: {}", err.describe()),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_replay_attributes_the_last_twenty_messages() {
        let texts: Vec<(String, String)> = (0..15)
            .map(|i| (format!("q{i}"), format!("a{i}")))
            .collect();
        let mut messages = Vec::new();
        for (q, a) in &texts {
            messages.push((Role::User, q.as_str()));
            messages.push((Role::Assistant, a.as_str()));
            messages.push((Role::Tool, "tool output"));
        }
        let text = replay("ses_x", &messages);
        assert!(text.starts_with("Continuing session ses_x here."));
        assert!(
            !text.contains("q4\n") && !text.contains("You: q4"),
            "{text}"
        );
        assert!(text.contains("You: q5") && text.ends_with("Assistant: a14"));
        assert!(!text.contains("tool output"));
    }
}
