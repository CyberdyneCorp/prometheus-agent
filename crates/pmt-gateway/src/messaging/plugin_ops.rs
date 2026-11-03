//! What plugins do through the gateway (spec: plugin-system, Plugin live
//! messages and message injection; Platform actions): live messages in a
//! session's own chat (posted, then edited in place where the adapter
//! edits), text injected into a session of the plugin's own profile
//! through its busy-input queue, and reactions and thread titles on
//! connected adapters of that profile. None of these hand out an adapter.

use super::Gateway;
use super::adapter::{Outgoing, Reaction, SendError};
use super::adapters::Op;
use super::key::KeyParts;
use super::lane::Msg;

/// Why a live message did not go out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostError {
    /// The session is not a conversation this gateway serves.
    NoConversation,
    /// The session's adapter is not connected.
    AdapterUnavailable,
    /// The platform refused the message.
    SendFailed,
    /// The platform refused the edit.
    EditFailed,
}

/// A posted or edited live message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posted {
    /// The platform's message id.
    pub handle: String,
    /// The adapter cannot edit: the text went out as a new message.
    pub fallback: bool,
}

/// Where injected text goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Injection {
    /// A session id.
    Session(String),
    /// A chat, `<platform>:<chat_id>`.
    Origin(String),
}

/// A refused platform action: an error code of the plugin `platform`
/// interface and its detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRefused {
    /// `unknown_platform`, `adapter_not_registered`,
    /// `adapter_disconnected`, `unsupported_platform_action` or
    /// `action_failed`.
    pub code: &'static str,
    /// What happened.
    pub detail: String,
}

fn refused(code: &'static str, detail: impl Into<String>) -> ActionRefused {
    ActionRefused {
        code,
        detail: detail.into(),
    }
}

impl Gateway {
    /// The route of `session_id` when it is a conversation of `profile`
    /// this gateway runs.
    fn session_route(&self, profile: &str, session_id: &str) -> Option<KeyParts> {
        let key = self
            .shared
            .sessions
            .lock()
            .expect("sessions poisoned")
            .get(session_id)
            .cloned()?;
        KeyParts::parse(&key).filter(|parts| parts.profile == profile)
    }

    /// `text` clamped to the platform's message limit.
    fn clamped(&self, profile: &str, platform: &str, text: &str) -> String {
        let limit = self
            .shared
            .adapters
            .descriptor(profile, platform)
            .map_or(usize::MAX, |d| d.max_message_length.max(1));
        text.chars().take(limit).collect()
    }

    /// Posts `text` in the chat of `session_id`.
    ///
    /// # Errors
    ///
    /// What kept it from going out.
    pub async fn post_live(
        &self,
        profile: &str,
        session_id: &str,
        text: &str,
    ) -> Result<Posted, PostError> {
        let route = self
            .session_route(profile, session_id)
            .ok_or(PostError::NoConversation)?;
        let slot = self
            .shared
            .adapters
            .ready(profile, &route.platform)
            .ok_or(PostError::AdapterUnavailable)?;
        let text = self.clamped(profile, &route.platform, text);
        let message = Outgoing::text(&route.chat_id, route.thread_id.as_deref(), &text);
        let sent = slot
            .adapter
            .send(&message)
            .await
            .map_err(|_| PostError::SendFailed)?;
        Ok(Posted {
            handle: sent.message_id.unwrap_or_default(),
            fallback: false,
        })
    }

    /// Replaces the text of live message `handle` in the chat of
    /// `session_id`; an adapter that cannot edit gets a new message
    /// (`fallback`).
    ///
    /// # Errors
    ///
    /// What kept it from going out.
    pub async fn edit_live(
        &self,
        profile: &str,
        session_id: &str,
        handle: &str,
        text: &str,
    ) -> Result<Posted, PostError> {
        let route = self
            .session_route(profile, session_id)
            .ok_or(PostError::NoConversation)?;
        let slot = self
            .shared
            .adapters
            .ready(profile, &route.platform)
            .ok_or(PostError::AdapterUnavailable)?;
        let text = self.clamped(profile, &route.platform, text);
        let message = Outgoing::text(&route.chat_id, route.thread_id.as_deref(), &text);
        if slot.can(Op::Edit) && !handle.is_empty() {
            return match slot.adapter.edit(handle, &message).await {
                Ok(()) => Ok(Posted {
                    handle: handle.to_owned(),
                    fallback: false,
                }),
                Err(SendError::Unsupported) => {
                    slot.refused(Op::Edit);
                    Err(PostError::EditFailed)
                }
                Err(_) => Err(PostError::EditFailed),
            };
        }
        let sent = slot
            .adapter
            .send(&message)
            .await
            .map_err(|_| PostError::SendFailed)?;
        Ok(Posted {
            handle: sent.message_id.unwrap_or_default(),
            fallback: true,
        })
    }

    /// Queues `text` as new input of a conversation of `profile`, after
    /// re-checking that its chat is still an authorized destination; an
    /// `origin` chat needs a conversation a person started.
    ///
    /// # Errors
    ///
    /// Why it was refused.
    ///
    /// # Panics
    ///
    /// When the sessions or lanes lock is poisoned.
    pub async fn inject(
        &self,
        profile: &str,
        target: &Injection,
        text: &str,
    ) -> Result<(), String> {
        let (key, parts) = match target {
            Injection::Session(id) => {
                let key = self
                    .shared
                    .sessions
                    .lock()
                    .expect("sessions poisoned")
                    .get(id)
                    .cloned()
                    .ok_or_else(|| format!("no running conversation has session '{id}'"))?;
                let parts = KeyParts::parse(&key).ok_or("unreadable session key")?;
                if parts.profile != profile {
                    return Err(format!("session '{id}' belongs to another profile"));
                }
                (key, parts)
            }
            Injection::Origin(origin) => {
                let (platform, chat) = origin
                    .split_once(':')
                    .ok_or_else(|| format!("origin '{origin}' is not <platform>:<chat_id>"))?;
                self.lane_of(profile, platform, chat).ok_or_else(|| {
                    format!("no person has started a conversation in {origin} yet")
                })?
            }
        };
        let destination = format!("{}:{}", parts.platform, parts.chat_id);
        let resolved = self
            .resolve_target(profile, &destination)
            .await
            .map_err(|e| e.message().to_owned())?;
        self.authorize_destination(profile, &resolved)
            .await
            .map_err(|e| e.message().to_owned())?;
        let handle = self
            .shared
            .lanes
            .lock()
            .expect("lanes poisoned")
            .get(&key)
            .cloned()
            .ok_or("the conversation is not running")?;
        handle.send(Msg::Injected(text.to_owned()));
        Ok(())
    }

    /// The running conversation of `platform:chat` in `profile`.
    fn lane_of(&self, profile: &str, platform: &str, chat: &str) -> Option<(String, KeyParts)> {
        self.shared
            .lanes
            .lock()
            .expect("lanes poisoned")
            .keys()
            .filter_map(|key| KeyParts::parse(key).map(|parts| (key.clone(), parts)))
            .find(|(_, p)| p.profile == profile && p.platform == platform && p.chat_id == chat)
    }

    /// Runs a platform action on a connected adapter of `profile`.
    ///
    /// # Errors
    ///
    /// The refusal's code and detail.
    pub async fn platform_action(
        &self,
        profile: &str,
        verb: &str,
        platform: &str,
        chat_id: &str,
        target_id: &str,
        value: &str,
    ) -> Result<(), ActionRefused> {
        let adapters = &self.shared.adapters;
        let Some(descriptor) = adapters.descriptor(profile, platform) else {
            let known = self
                .shared
                .gateway_config(profile)
                .is_ok_and(|g| g.platforms.contains_key(platform));
            return Err(if known {
                refused(
                    "adapter_not_registered",
                    format!("no {platform} adapter runs for this profile"),
                )
            } else {
                refused(
                    "unknown_platform",
                    format!("'{platform}' is not a platform of this profile"),
                )
            });
        };
        let slot = adapters.ready(profile, platform).ok_or_else(|| {
            refused(
                "adapter_disconnected",
                format!("the {platform} adapter is not connected"),
            )
        })?;
        let outcome = match verb {
            "add_reaction" if descriptor.supports_reactions => {
                let reaction = Reaction {
                    chat_id: chat_id.to_owned(),
                    message_id: target_id.to_owned(),
                    emoji: value.to_owned(),
                    remove: false,
                };
                slot.adapter.react(&reaction).await
            }
            "set_thread_title" if descriptor.supports_threads => {
                slot.adapter.rename_thread(chat_id, target_id, value).await
            }
            _ => {
                return Err(refused(
                    "unsupported_platform_action",
                    format!("the {platform} adapter does not support {verb}"),
                ));
            }
        };
        outcome.map_err(|err| match err {
            SendError::Unsupported => refused(
                "unsupported_platform_action",
                format!("the {platform} adapter does not support {verb}"),
            ),
            other => refused("action_failed", other.describe()),
        })
    }
}
