//! One relayed platform of one profile, as the gateway's adapter slots see
//! it: sends and edits go through the shared connection, the descriptor
//! comes from the connector, and connecting means waiting for the relay.

use std::sync::{Arc, Weak};

use super::Relay;
use super::wire::DEFAULT_MAX_LENGTH;
use crate::messaging::adapter::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterState, Attachment, ChatInfo,
    Descriptor, MediaStream, Outgoing, SendError, Sent,
};

/// The relay as the adapter of `platform`.
pub(crate) struct RelayView {
    relay: Weak<Relay>,
    profile: String,
    platform: String,
}

impl RelayView {
    pub(crate) fn new(relay: &Arc<Relay>, profile: &str, platform: &str) -> Self {
        Self {
            relay: Arc::downgrade(relay),
            profile: profile.to_owned(),
            platform: platform.to_owned(),
        }
    }

    fn key(&self) -> (&str, &str) {
        (&self.profile, &self.platform)
    }

    fn relay(&self) -> Result<Arc<Relay>, SendError> {
        self.relay
            .upgrade()
            .ok_or_else(|| SendError::Network("the relay stopped".to_owned()))
    }
}

impl Adapter for RelayView {
    fn reach(&self) -> Result<crate::messaging::probe::Reach, String> {
        let relay = self.relay.upgrade().ok_or("the relay stopped")?;
        if relay.is_attached() {
            // The host dials in: nothing here to probe.
            return Ok(crate::messaging::probe::Reach::InboundOnly);
        }
        Ok(crate::messaging::probe::Reach::Endpoints(vec![
            crate::messaging::probe::Endpoint::Http(relay.url().to_owned()),
        ]))
    }

    fn describe(&self) -> Descriptor {
        let declared = self.relay.upgrade().and_then(|r| r.descriptor());
        Descriptor {
            // Only the relay may trust its connector's authorization.
            authorization_is_upstream: true,
            ..declared.map_or_else(
                || Descriptor::new(&self.platform, DEFAULT_MAX_LENGTH),
                |d| d.descriptor_of(&self.platform),
            )
        }
    }

    fn open_media<'a>(
        &'a self,
        attachment: &'a Attachment,
    ) -> AdapterFuture<'a, Result<MediaStream, SendError>> {
        Box::pin(async move { self.relay()?.media(self.key(), &attachment.location).await })
    }

    fn connect(&self, link: Arc<dyn AdapterLink>) -> AdapterFuture<'_, Result<(), AdapterError>> {
        Box::pin(async move {
            let Some(relay) = self.relay.upgrade() else {
                return Err(AdapterError {
                    retryable: false,
                    code: "relay_down".to_owned(),
                    message: "the relay stopped".to_owned(),
                });
            };
            relay.wait_connected().await;
            relay.watch_link(Arc::clone(&link));
            link.report(AdapterState::Connected, None);
            Ok(())
        })
    }

    fn disconnect(&self) -> AdapterFuture<'_, ()> {
        Box::pin(async {})
    }

    fn send(&self, message: &Outgoing) -> AdapterFuture<'_, Result<Sent, SendError>> {
        let message = message.clone();
        Box::pin(async move {
            let message_id = self.relay()?.send(self.key(), &message, None).await?;
            Ok(Sent { message_id })
        })
    }

    fn chat_info<'a>(&'a self, chat_id: &'a str) -> AdapterFuture<'a, Result<ChatInfo, SendError>> {
        // The connector does not describe chats: the id is the name.
        Box::pin(async move {
            Ok(ChatInfo {
                name: chat_id.to_owned(),
                chat_type: crate::messaging::key::ChatType::Group,
            })
        })
    }

    fn edit<'a>(
        &'a self,
        message_id: &'a str,
        message: &'a Outgoing,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            self.relay()?
                .send(self.key(), message, Some(message_id))
                .await
                .map(|_| ())
        })
    }
}
