//! Replies only one member of a chat may see (spec: messaging-gateway,
//! Approval requests routed to every surface, Bystander cannot approve):
//! a refusal to someone who may not decide a prompt goes to them alone,
//! through the adapter's `reply_privately` (ephemeral where the platform
//! has it, else a direct message). In a direct message the chat already
//! is private; the reply is posted in the chat only when the platform can
//! do neither.

use pmt_store::gateway::outbox::OutboundKind;
use tracing::Instrument as _;

use super::Lane;
use crate::messaging::adapter::Private;
use crate::messaging::key::{ChatType, Routing};

impl Lane {
    /// Tells the sender of `routing` alone `text`.
    pub(super) fn say_privately(&self, routing: &Routing, text: &str) {
        let adapter = (routing.chat_type != ChatType::Dm)
            .then(|| {
                self.shared
                    .adapters
                    .descriptor(&self.profile, &routing.platform)
            })
            .flatten()
            .filter(|d| d.private_replies)
            .and_then(|_| {
                self.shared
                    .adapters
                    .adapter(&self.profile, &routing.platform)
            });
        let Some(adapter) = adapter else {
            return self.say(text);
        };
        let private = Private {
            chat_id: routing.chat_id.clone(),
            thread_id: routing.thread_id.clone(),
            sender_id: routing.sender_id.clone(),
            interaction: self.interaction.clone(),
            text: text.to_owned(),
        };
        let key = format!("system:{}", crate::tickets::random_hex(8));
        let public = self.outbound(key, OutboundKind::System, text.to_owned(), None);
        let shared = std::sync::Arc::clone(&self.shared);
        let profile = self.profile.clone();
        tokio::spawn(
            async move {
                if let Err(err) = adapter.reply_privately(&private).await {
                    tracing::info!(
                        "private reply not possible ({}); posting it in the chat",
                        err.describe()
                    );
                    shared.deliver(&profile, &public);
                }
            }
            .in_current_span(),
        );
    }
}
