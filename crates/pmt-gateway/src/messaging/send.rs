//! Messages the gateway sends without an agent turn (spec: messaging-
//! gateway, Home channel and /sethome; agent-interop, One-shot send
//! command; `send_message` tool): a target is resolved against the
//! profile's configuration (a bare platform is its home channel), the
//! message goes through the durable outbox, and the `send_message` tool's
//! destinations are authorized first, failing closed.

use std::collections::HashMap;
use std::sync::Mutex;

use pmt_config::schema::GatewayConfig;
use pmt_core::error::{Error, codes};
use pmt_store::gateway::outbox::{NewOutbound, Outbound, OutboundAttachment, OutboundKind};
use pmt_store::{NewMessage, Role, Store};
use serde::Serialize;
use serde_json::json;

use super::adapter::{Descriptor, Reaction, SendError};
use super::shape::{self, Owner};
use super::target::{self, Destination, Resolved, Target};
use super::{Gateway, Shared};

/// One message to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendRequest {
    /// The target (`platform`, `platform:chat_id[:thread_id]`,
    /// `platform:#name`).
    pub target: String,
    /// The text.
    pub text: String,
    /// Files sent with it.
    pub attachments: Vec<OutboundAttachment>,
    /// An automatic warning or failure notice (a failed scheduled job, a
    /// kanban failure): muted where `suppress_warning_notifications` is on.
    pub warning: bool,
}

/// A destination `gateway send --list` shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KnownTarget {
    /// The target as `send --to` takes it.
    pub target: String,
    /// Why it is known: `home`, `allowed` or `session`.
    pub kind: &'static str,
    /// The home channel's display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// The known destinations of a profile: home channels, allowed chats and
/// chats with sessions (deduplicated, in that order of precedence).
///
/// # Errors
///
/// `store.sqlite`.
pub fn known_targets(
    gateway: &GatewayConfig,
    store: &Store,
    platform: Option<&str>,
) -> Result<Vec<KnownTarget>, Error> {
    let mut found: Vec<KnownTarget> = Vec::new();
    let mut push = |target: String, kind, name| {
        if !found.iter().any(|t| t.target == target) {
            found.push(KnownTarget { target, kind, name });
        }
    };
    let wanted = |p: &str| platform.is_none_or(|w| w == p);
    for (name, table) in gateway.platforms.iter().filter(|(p, _)| wanted(p)) {
        if let Some(home) = &table.home_channel {
            push(
                format!("{name}:{}", home.chat_target()),
                "home",
                home.name.clone(),
            );
        }
    }
    for (name, table) in gateway.platforms.iter().filter(|(p, _)| wanted(p)) {
        for chat in &table.allow_chats {
            push(format!("{name}:{chat}"), "allowed", None);
        }
    }
    for chat in store.routed_chats(platform)? {
        let target = Resolved {
            platform: chat.platform,
            chat_id: chat.chat_id,
            thread_id: chat.thread_id,
        };
        push(target.label(), "session", None);
    }
    Ok(found)
}

/// The newest inbound message per chat (what a reaction targets by
/// default).
#[derive(Debug, Default)]
pub(crate) struct LastInbound(Mutex<HashMap<(String, String, String), String>>);

impl LastInbound {
    pub(crate) fn record(&self, profile: &str, platform: &str, chat: &str, message_id: &str) {
        if message_id.is_empty() {
            return;
        }
        self.0.lock().expect("last inbound poisoned").insert(
            (profile.to_owned(), platform.to_owned(), chat.to_owned()),
            message_id.to_owned(),
        );
    }

    fn get(&self, profile: &str, platform: &str, chat: &str) -> Option<String> {
        self.0
            .lock()
            .expect("last inbound poisoned")
            .get(&(profile.to_owned(), platform.to_owned(), chat.to_owned()))
            .cloned()
    }
}

/// Appends a mirrored message to `session`.
pub(crate) fn append_mirror(
    store: &Store,
    session: pmt_core::id::SessionId,
    text: &str,
    from: Option<&str>,
) {
    let mut message = NewMessage::new(Role::Assistant, text);
    message.metadata = Some(json!({"mirrored": {"via": "send_message", "from_session": from}}));
    if let Err(err) = store.append_message(session, &message) {
        tracing::warn!("send_message not mirrored: {err}");
    }
}

fn refused(target: &Resolved) -> Error {
    Error::new(
        &codes::GATEWAY_TARGET_REFUSED,
        format!("target {} is not an authorized destination", target.label()),
    )
}

impl Shared {
    fn gateway_config(&self, profile: &str) -> Result<GatewayConfig, Error> {
        self.agents
            .scope(profile)
            .map(|s| s.config().gateway.clone())
            .ok_or_else(|| {
                Error::new(
                    &codes::PROFILE_NOT_FOUND,
                    format!("profile '{profile}' is not served"),
                )
            })
    }

    fn profile_store(&self, profile: &str) -> Result<std::sync::Arc<Store>, Error> {
        self.store(profile).ok_or_else(|| {
            Error::new(
                &codes::PROFILE_NOT_FOUND,
                format!("profile '{profile}' is not served"),
            )
        })
    }
}

impl Gateway {
    /// The known destinations of `profile` (home channels, allowed chats,
    /// chats with sessions), optionally of one platform.
    ///
    /// # Errors
    ///
    /// `profile.not_found`, store errors.
    pub fn known_targets(
        &self,
        profile: &str,
        platform: Option<&str>,
    ) -> Result<Vec<KnownTarget>, Error> {
        let gateway = self.shared.gateway_config(profile)?;
        let store = self.shared.profile_store(profile)?;
        known_targets(&gateway, &store, platform)
    }

    /// Resolves `target` for `profile`: the platform must be enabled (or
    /// carried by the relay), a bare platform is its home channel and
    /// `#name` a home channel or a channel the live adapter knows.
    ///
    /// # Errors
    ///
    /// `gateway.unknown_target` (66), `gateway.no_home_channel` (66),
    /// `gateway.platform_not_configured` (78).
    pub async fn resolve_target(&self, profile: &str, target: &str) -> Result<Resolved, Error> {
        let shared = &self.shared;
        let parsed = Target::parse(target)?;
        let gateway = shared.gateway_config(profile)?;
        let running = shared
            .adapters
            .descriptor(profile, &parsed.platform)
            .is_some();
        if !running && !target::configured(&gateway, &parsed.platform) {
            return Err(target::not_configured(&parsed.platform));
        }
        if let Some(resolved) = target::resolve(&gateway, &parsed)? {
            return Ok(resolved);
        }
        let Destination::Named(name) = &parsed.destination else {
            unreachable!("only a #name can stay unresolved");
        };
        let chat = match shared.adapters.ready(profile, &parsed.platform) {
            Some(slot) => slot.adapter.resolve_channel(name).await,
            None => None,
        };
        chat.map(|chat_id| Resolved {
            platform: parsed.platform.clone(),
            chat_id,
            thread_id: None,
        })
        .ok_or_else(|| target::unknown_channel(&parsed.platform, name))
    }

    /// Queues one message for `target` in `profile`'s outbox (no agent
    /// turn) and wakes the delivery worker.
    ///
    /// # Errors
    ///
    /// As [`resolve_target`](Self::resolve_target); store errors.
    pub async fn send(&self, profile: &str, request: &SendRequest) -> Result<Outbound, Error> {
        let resolved = self.resolve_target(profile, &request.target).await?;
        let kind = if request.warning {
            OutboundKind::Warning
        } else {
            OutboundKind::Notification
        };
        self.enqueue_as(
            profile,
            &resolved,
            &request.text,
            &request.attachments,
            kind,
        )
    }

    /// Queues one message for an already resolved destination: split at
    /// the platform's limit, an oversized text truncated with the full
    /// text attached; the first entry is returned.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn enqueue(
        &self,
        profile: &str,
        to: &Resolved,
        text: &str,
        attachments: &[OutboundAttachment],
    ) -> Result<Outbound, Error> {
        self.enqueue_as(profile, to, text, attachments, OutboundKind::Notification)
    }

    fn enqueue_as(
        &self,
        profile: &str,
        to: &Resolved,
        text: &str,
        attachments: &[OutboundAttachment],
        kind: OutboundKind,
    ) -> Result<Outbound, Error> {
        let store = self.shared.profile_store(profile)?;
        let key = format!("send:{}", crate::tickets::random_hex(12));
        let descriptor = self
            .shared
            .adapters
            .descriptor(profile, &to.platform)
            .unwrap_or_else(|| {
                let limit = pmt_config::platforms::first_party(&to.platform)
                    .map_or(4096, |p| p.max_message_length as usize);
                Descriptor::new(&to.platform, limit)
            });
        let owner = Owner {
            session: None,
            key: &key,
        };
        let parts = shape::parts(&self.shared, profile, &descriptor, text, &owner);
        let count = parts.len();
        let mut first = None;
        for (index, (body, mut attached)) in parts.into_iter().enumerate() {
            if index + 1 == count {
                attached.extend(attachments.iter().cloned());
            }
            let new = NewOutbound {
                idempotency_key: if index == 0 {
                    key.clone()
                } else {
                    format!("{key}:{index}")
                },
                session_id: None,
                platform: to.platform.clone(),
                chat_id: to.chat_id.clone(),
                thread_id: to.thread_id.clone(),
                reply_to: None,
                edit_of: None,
                kind,
                body,
                attachments: attached,
            };
            let (entry, _) = store.enqueue_outbound(&new)?;
            first.get_or_insert(entry);
        }
        self.shared.outbox_wake.notify_waiters();
        first.ok_or_else(|| Error::new(&codes::GATEWAY_DELIVERY_FAILED, "nothing to send"))
    }

    /// Whether the `send_message` tool may reach `to`: the platform's home
    /// channel, a chat in `allow_chats` or a chat the profile has a
    /// session in, and for a relayed platform the connector's egress
    /// verdict. Any failure while checking refuses.
    ///
    /// # Errors
    ///
    /// `gateway.target_refused`.
    pub async fn authorize_destination(&self, profile: &str, to: &Resolved) -> Result<(), Error> {
        let shared = &self.shared;
        let Ok(gateway) = shared.gateway_config(profile) else {
            return Err(refused(to));
        };
        let table = gateway.platform(&to.platform);
        let home = table
            .home_channel
            .as_ref()
            .is_some_and(|h| h.chat_id == to.chat_id);
        let allowed = table.allow_chats.contains(&to.chat_id);
        let known = || -> Result<bool, Error> {
            let store = shared.profile_store(profile)?;
            Ok(store
                .routed_chats(Some(&to.platform))?
                .iter()
                .any(|c| c.chat_id == to.chat_id))
        };
        let listed = home
            || allowed
            || known().unwrap_or_else(|err| {
                tracing::warn!("destination check failed, refusing: {err}");
                false
            });
        if !listed {
            return Err(refused(to));
        }
        let relayed = shared
            .adapters
            .slot_relayed(profile, &to.platform)
            .unwrap_or_else(|| shared.relay.get().is_some_and(|r| r.fronts(&to.platform)));
        if relayed {
            let Some(relay) = shared.relay.get() else {
                return Err(refused(to));
            };
            match relay.authorize_egress(&to.platform, &to.chat_id).await {
                Ok(true) => {}
                Ok(false) => return Err(refused(to)),
                Err(err) => {
                    tracing::warn!("relay egress check failed, refusing: {err}");
                    return Err(refused(to));
                }
            }
        }
        Ok(())
    }

    /// Mirrors a message sent to `to` into the session that chat routes
    /// to, as an attributed assistant message: none when the chat has no
    /// session or it is the sending session (whose tool call records it),
    /// after the running turn when that session is busy.
    pub fn mirror(&self, profile: &str, to: &Resolved, text: &str, from_session: Option<&str>) {
        let Ok(store) = self.shared.profile_store(profile) else {
            return;
        };
        let session = match store.chat_session(&to.platform, &to.chat_id, to.thread_id.as_deref()) {
            Ok(Some(session)) => session,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!("send_message not mirrored: {err}");
                return;
            }
        };
        let id = session.to_string();
        if from_session == Some(id.as_str()) {
            return;
        }
        let lane = self.shared.lane_of_session(&id);
        match lane {
            Some(lane) => lane.send(super::lane::Msg::Mirror {
                text: text.to_owned(),
                from_session: from_session.map(str::to_owned),
            }),
            None => append_mirror(&store, session, text, from_session),
        }
    }

    /// Adds or removes a reaction in `to` on `message_id` (default: the
    /// newest message received there); needs a live adapter that supports
    /// reactions.
    ///
    /// # Errors
    ///
    /// `gateway.no_adapter`, `gateway.delivery_failed`.
    pub async fn react(
        &self,
        profile: &str,
        to: &Resolved,
        message_id: Option<&str>,
        emoji: &str,
        remove: bool,
    ) -> Result<(), Error> {
        let shared = &self.shared;
        let slot = shared
            .adapters
            .ready(profile, &to.platform)
            .filter(|s| s.can(super::adapters::Op::Reactions))
            .ok_or_else(|| {
                Error::new(
                    &codes::GATEWAY_NO_ADAPTER,
                    format!(
                        "no connected {} adapter that supports reactions",
                        to.platform
                    ),
                )
            })?;
        let message_id = message_id
            .map(str::to_owned)
            .or_else(|| shared.last_inbound.get(profile, &to.platform, &to.chat_id))
            .ok_or_else(|| {
                Error::new(
                    &codes::GATEWAY_DELIVERY_FAILED,
                    format!("no message to react to in {}", to.label()),
                )
            })?;
        let reaction = Reaction {
            chat_id: to.chat_id.clone(),
            message_id,
            emoji: emoji.to_owned(),
            remove,
        };
        slot.adapter
            .react(&reaction)
            .await
            .map_err(|err| match err {
                SendError::Unsupported => {
                    slot.refused(super::adapters::Op::Reactions);
                    Error::new(
                        &codes::GATEWAY_NO_ADAPTER,
                        format!("the {} adapter does not support reactions", to.platform),
                    )
                }
                other => Error::new(&codes::GATEWAY_DELIVERY_FAILED, other.describe()),
            })
    }
}
