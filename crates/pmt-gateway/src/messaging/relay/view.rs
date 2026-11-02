//! One relayed platform of one profile, as the gateway's adapter slots see
//! it: sends and edits go through the shared connection, the descriptor
//! comes from the connector, and connecting means waiting for the relay.

use std::sync::{Arc, Weak};

use super::Relay;
use super::wire::DEFAULT_MAX_LENGTH;
use crate::messaging::adapter::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterState, Descriptor, Outgoing,
    SendError, Sent,
};

/// The relay as the adapter of `platform`.
pub(crate) struct RelayView {
    relay: Weak<Relay>,
    platform: String,
}

impl RelayView {
    pub(crate) fn new(relay: &Arc<Relay>, platform: &str) -> Self {
        Self {
            relay: Arc::downgrade(relay),
            platform: platform.to_owned(),
        }
    }

    fn relay(&self) -> Result<Arc<Relay>, SendError> {
        self.relay
            .upgrade()
            .ok_or_else(|| SendError::Network("the relay stopped".to_owned()))
    }
}

impl Adapter for RelayView {
    fn describe(&self) -> Descriptor {
        let declared = self.relay.upgrade().and_then(|r| r.descriptor());
        Descriptor {
            supports_edit: declared.as_ref().is_some_and(|d| d.supports_edit),
            // Only the relay may trust its connector's authorization.
            authorization_is_upstream: true,
            ..Descriptor::new(
                &self.platform,
                declared.map_or(DEFAULT_MAX_LENGTH, |d| d.max_message_length),
            )
        }
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
            let message_id = self.relay()?.send(&self.platform, &message, None).await?;
            Ok(Sent { message_id })
        })
    }

    fn edit<'a>(
        &'a self,
        message_id: &'a str,
        message: &'a Outgoing,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            self.relay()?
                .send(&self.platform, message, Some(message_id))
                .await
                .map(|_| ())
        })
    }
}
