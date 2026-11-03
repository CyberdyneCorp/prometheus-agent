//! MCP server notifications as internal webhook events (spec:
//! messaging-gateway, Webhook route delivery and safe default toolset).
//! With `gateway.mcp_events.enabled` (default off, experimental), the MCP
//! client hands a notification of a server that declares an `events`
//! route to [`Gateway::mcp_event`]; it reaches that webhook route as a
//! request the gateway itself vouches for (never through the HTTP
//! listener), so the route's filters, script, prompt, delivery and
//! toolset rules apply exactly as to a signed webhook request.

use pmt_core::error::{Error, codes};

use super::Gateway;
use super::adapter::{HttpRequest, HttpResponse};

/// The header naming an internal event's type (the webhook adapter reads
/// it like `X-GitHub-Event`).
pub const EVENT_HEADER: &str = "x-prometheus-event";

impl Gateway {
    /// Admits `payload` (an MCP server's notification of type `event`) on
    /// webhook route `route` of `profile`; the route's HTTP answer.
    ///
    /// # Errors
    ///
    /// `config.invalid` while `gateway.mcp_events.enabled` is off;
    /// `gateway.no_adapter` when the profile runs no webhook adapter.
    pub async fn mcp_event(
        &self,
        profile: &str,
        route: &str,
        event: &str,
        delivery_id: &str,
        payload: &serde_json::Value,
    ) -> Result<HttpResponse, Error> {
        let enabled = self
            .shared
            .agents
            .scope(profile)
            .is_some_and(|s| s.config().gateway.mcp_events.enabled);
        if !enabled {
            return Err(Error::new(
                &codes::CONFIG_INVALID,
                "MCP events are off; set gateway.mcp_events.enabled = true",
            ));
        }
        let slot = self
            .shared
            .adapters
            .ready(profile, "webhook")
            .ok_or_else(|| {
                Error::new(
                    &codes::GATEWAY_NO_ADAPTER,
                    format!("profile '{profile}' runs no webhook adapter"),
                )
            })?;
        let request = HttpRequest {
            method: "POST".to_owned(),
            path: format!("/{route}"),
            query: None,
            headers: vec![
                (EVENT_HEADER.to_owned(), event.to_owned()),
                ("x-request-id".to_owned(), delivery_id.to_owned()),
                ("content-type".to_owned(), "application/json".to_owned()),
            ],
            body: payload.to_string().into_bytes(),
        };
        tracing::info!(
            profile,
            route,
            event,
            "MCP event admitted on a webhook route"
        );
        Ok(slot.adapter.handle_http(&request).await)
    }
}
