//! The plugins' messaging port in the daemon (spec: plugin-system, Plugin
//! live messages and message injection; Platform actions): live messages
//! go to the calling session's own chat through the gateway, injected
//! text into a conversation of the plugin's own profile, and platform
//! actions to that profile's connected adapters. Outside the daemon
//! there is no gateway, and every call answers so.

use std::sync::{Arc, OnceLock};

use pmt_gateway::messaging::Gateway;
use pmt_gateway::messaging::plugin_ops::{Injection, PostError};
use pmt_plugins::host::ports::{
    ActionError, InjectTarget, MessagingPort, PlatformAction, PortFuture, PostFailure, Posted,
};

/// The daemon's gateway, once started (one per process).
static GATEWAY: OnceLock<Arc<OnceLock<Gateway>>> = OnceLock::new();

/// Hands plugins the daemon's gateway cell.
pub fn set_gateway(cell: Arc<OnceLock<Gateway>>) {
    let _ = GATEWAY.set(cell);
}

fn gateway() -> Option<&'static Gateway> {
    GATEWAY.get()?.get()
}

/// The messaging port of one profile's plugins.
pub(super) struct GatewayMessaging {
    pub(super) profile: String,
}

fn failure(error: PostError) -> PostFailure {
    match error {
        PostError::NoConversation => PostFailure::NoConversation,
        PostError::AdapterUnavailable => PostFailure::AdapterUnavailable,
        PostError::SendFailed => PostFailure::SendFailed,
        PostError::EditFailed => PostFailure::EditFailed,
    }
}

fn posted(
    result: Result<pmt_gateway::messaging::plugin_ops::Posted, PostError>,
) -> Result<Posted, PostFailure> {
    result
        .map(|p| Posted {
            handle: p.handle,
            fallback: p.fallback,
        })
        .map_err(failure)
}

impl MessagingPort for GatewayMessaging {
    fn post(&self, session_id: &str, text: &str) -> PortFuture<'_, Result<Posted, PostFailure>> {
        let (session_id, text) = (session_id.to_owned(), text.to_owned());
        Box::pin(async move {
            let gateway = gateway().ok_or(PostFailure::GatewayUnavailable)?;
            posted(gateway.post_live(&self.profile, &session_id, &text).await)
        })
    }

    fn edit(
        &self,
        session_id: &str,
        handle: &str,
        text: &str,
    ) -> PortFuture<'_, Result<Posted, PostFailure>> {
        let (session_id, handle, text) =
            (session_id.to_owned(), handle.to_owned(), text.to_owned());
        Box::pin(async move {
            let gateway = gateway().ok_or(PostFailure::GatewayUnavailable)?;
            posted(
                gateway
                    .edit_live(&self.profile, &session_id, &handle, &text)
                    .await,
            )
        })
    }

    fn inject(&self, target: &InjectTarget, content: &str) -> PortFuture<'_, Result<(), String>> {
        let target = match target {
            InjectTarget::SessionKey(key) => Injection::Session(key.clone()),
            InjectTarget::Origin(origin) => Injection::Origin(origin.clone()),
        };
        let content = content.to_owned();
        Box::pin(async move {
            let gateway = gateway().ok_or("the messaging gateway is not running")?;
            gateway.inject(&self.profile, &target, &content).await
        })
    }

    fn platform_action(&self, action: &PlatformAction) -> PortFuture<'_, Result<(), ActionError>> {
        let action = action.clone();
        Box::pin(async move {
            let gateway = gateway().ok_or(ActionError {
                code: "gateway_unavailable",
                detail: "the messaging gateway is not running".to_owned(),
            })?;
            gateway
                .platform_action(
                    &self.profile,
                    action.verb,
                    &action.platform,
                    &action.chat_id,
                    &action.target_id,
                    &action.value,
                )
                .await
                .map_err(|refused| ActionError {
                    code: refused.code,
                    detail: refused.detail,
                })
        })
    }
}
