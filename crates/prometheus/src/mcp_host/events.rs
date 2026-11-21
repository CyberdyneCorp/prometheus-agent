//! Where a profile's MCP event subscriptions are delivered (spec:
//! plugin-system, MCP events): the webhook adapter's public URL, and a
//! notice to a route's `deliver` target when its server ends the
//! subscription, queued in the profile's gateway outbox so the daemon
//! sends it like any other system message.

use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_gateway::messaging::target::{self, Target};
use pmt_store::gateway::outbox::{NewOutbound, OutboundKind};

/// The events settings of `scope`'s servers.
pub(super) fn of(scope: &ProfileScope) -> pmt_mcp::events::Events {
    let options = scope.config().gateway.platform("webhook").options;
    let public_url = pmt_config::platforms::options::<pmt_config::platforms::WebhookOptions>(
        "webhook", &options,
    )
    .ok()
    .and_then(|o| o.public_url);
    let owner = scope.clone();
    pmt_mcp::events::Events {
        scope: scope.clone(),
        public_url,
        notify: Arc::new(move |to: &str, text: &str| {
            if let Err(err) = queue(&owner, to, text) {
                tracing::warn!(target = to, "MCP event notice not queued: {err}");
            }
        }),
    }
}

fn queue(scope: &ProfileScope, to: &str, text: &str) -> Result<(), pmt_core::error::Error> {
    let parsed = Target::parse(to)?;
    let resolved = target::resolve(&scope.config().gateway, &parsed)?
        .ok_or_else(|| target::no_home_channel(&parsed.platform))?;
    let store = pmt_store::Store::open(scope)?;
    store.enqueue_outbound(&NewOutbound {
        idempotency_key: format!(
            "mcp-event-ended:{to}:{}",
            jiff::Timestamp::now().as_millisecond()
        ),
        session_id: None,
        platform: resolved.platform,
        chat_id: resolved.chat_id,
        thread_id: resolved.thread_id,
        reply_to: None,
        edit_of: None,
        kind: OutboundKind::System,
        body: text.to_owned(),
        attachments: Vec::new(),
        notify: None,
    })?;
    Ok(())
}
