//! The client side of one connection: what a server may ask of
//! Prometheus (spec: plugin-system, MCP sampling; MCP elicitation;
//! tool-system, MCP tool list changes respect the frozen surface).
//!
//! The handler holds only a weak reference to its server (the server
//! owns the connection that owns the handler). Requests and notifications
//! are answered by the server ([`crate::server::Server`]), which knows its
//! settings and the tool calls in flight.

#![allow(deprecated)] // sampling is deprecated upstream (SEP-2577) but specified here

use std::sync::Weak;

use rmcp::model::{
    ClientCapabilities, ClientConfig, CreateMessageRequestParams, CreateMessageResult,
    ElicitRequestParams, ElicitResult, ElicitationAction, Implementation,
};
use rmcp::service::{NotificationContext, RequestContext, RoleClient};
use rmcp::{ClientHandler, ErrorData};

use crate::host::ElicitAnswer;
use crate::server::Server;

/// The handler of one connection.
#[derive(Clone)]
pub struct Handler {
    server: Weak<Server>,
    capabilities: ClientCapabilities,
}

impl Handler {
    /// The handler of `server`'s connection, advertising sampling and
    /// form elicitation only when the server's settings enable them.
    #[must_use]
    pub fn new(server: Weak<Server>, sampling: bool, elicitation: bool) -> Self {
        let mut capabilities = serde_json::Map::new();
        if sampling {
            capabilities.insert("sampling".to_owned(), serde_json::json!({}));
        }
        if elicitation {
            capabilities.insert("elicitation".to_owned(), serde_json::json!({"form": {}}));
        }
        let capabilities =
            serde_json::from_value(serde_json::Value::Object(capabilities)).unwrap_or_default();
        Self {
            server,
            capabilities,
        }
    }

    fn gone() -> ErrorData {
        ErrorData::internal_error("the MCP server entry was removed", None)
    }
}

impl ClientHandler for Handler {
    async fn create_message(
        &self,
        params: CreateMessageRequestParams,
        _: RequestContext<RoleClient>,
    ) -> Result<CreateMessageResult, ErrorData> {
        let server = self.server.upgrade().ok_or_else(Self::gone)?;
        server.sample(params).await
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        let Some(server) = self.server.upgrade() else {
            return Ok(ElicitResult::new(ElicitationAction::Decline));
        };
        let answer = server.elicit(request).await;
        Ok(match answer {
            ElicitAnswer::Accept(content) => ElicitResult::new(ElicitationAction::Accept)
                .with_content(serde_json::Value::Object(content)),
            ElicitAnswer::Decline => ElicitResult::new(ElicitationAction::Decline),
            ElicitAnswer::Cancel => ElicitResult::new(ElicitationAction::Cancel),
        })
    }

    async fn on_tool_list_changed(&self, _: NotificationContext<RoleClient>) {
        if let Some(server) = self.server.upgrade() {
            // Never refresh inline: the refresh's own request travels over
            // this connection.
            pmt_core::spawn::spawn(async move { server.tools_list_changed().await });
        }
    }

    fn get_info(&self) -> ClientConfig {
        let mut config = ClientConfig::default();
        config.capabilities = self.capabilities.clone();
        config.client_info = Implementation::new("prometheus", env!("CARGO_PKG_VERSION"));
        config
    }
}
