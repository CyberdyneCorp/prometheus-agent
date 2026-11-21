//! MCP event deliveries (spec: platform-adapters, MCP server events into
//! the webhook pipeline): a route created by `subscribe_event` carries an
//! `mcp_event` block and is signed with the Standard Webhooks scheme. A
//! body with a top-level `type` is a control envelope, answered here
//! without a turn: `verification` echoes its challenge, `terminated`
//! removes the route and tells its `deliver` target, `gap` resumes from
//! the fresh cursor it carries, and an unknown type is acknowledged so the
//! server stops retrying it. Anything else is an event occurrence: its
//! replay cursor is remembered and it continues through the route's
//! normal pipeline, deduplicated by `mcp-event:<eventId>`.

use pmt_config::mcp_event_state;
use pmt_config::schema::{McpEventRoute, WebhookRoute};
use pmt_gateway::messaging::HttpResponse;
use serde_json::{Value, json};

use crate::{PLATFORM, Webhook, status};

/// The longest verification challenge echoed.
const MAX_CHALLENGE_CHARS: usize = 512;
/// The longest server-given reason a termination notice quotes.
const MAX_NOTICE_CHARS: usize = 200;

/// The idempotency key of an occurrence (stable across the server's
/// retries, unlike the per-attempt `webhook-id`).
pub(crate) fn delivery_id(payload: &Value) -> Option<String> {
    payload["eventId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(|id| format!("mcp-event:{id}"))
}

impl Webhook {
    /// The answer to a control envelope on MCP event route `name`; `None`
    /// lets an occurrence continue (after remembering its cursor).
    pub(crate) async fn mcp_control(
        &self,
        name: &str,
        route: &WebhookRoute,
        event: &McpEventRoute,
        payload: &Value,
    ) -> Option<HttpResponse> {
        let Some(kind) = payload.get("type") else {
            self.store_cursor(name, payload);
            return None;
        };
        Some(match kind.as_str() {
            Some("verification") => verification(payload),
            Some("terminated") => self.terminated(name, route, event, payload).await,
            Some("gap") => {
                self.store_cursor(name, payload);
                tracing::warn!(
                    platform = PLATFORM,
                    route = name,
                    "the MCP server skipped events (gap); resuming from a fresh cursor"
                );
                status(200, &json!({"status": "gap"}))
            }
            other => {
                tracing::info!(
                    platform = PLATFORM,
                    route = name,
                    "ignoring an MCP control envelope of unknown type {other:?}"
                );
                status(200, &json!({"status": "ignored", "type": other}))
            }
        })
    }

    fn store_cursor(&self, name: &str, payload: &Value) {
        let Some(cursor) = payload["cursor"].as_str().filter(|c| !c.is_empty()) else {
            return;
        };
        if mcp_event_state::get(self.scope.dir(), name)
            .cursor
            .as_deref()
            == Some(cursor)
        {
            return;
        }
        if let Err(e) = mcp_event_state::update(self.scope.dir(), name, |s| {
            s.cursor = Some(cursor.to_owned());
        }) {
            tracing::warn!(
                platform = PLATFORM,
                route = name,
                "MCP event cursor not stored: {e}"
            );
        }
    }

    async fn terminated(
        &self,
        name: &str,
        route: &WebhookRoute,
        event: &McpEventRoute,
        payload: &Value,
    ) -> HttpResponse {
        let error = &payload["error"];
        tracing::warn!(
            platform = PLATFORM,
            route = name,
            server = event.server,
            event = event.name,
            "the MCP server ended the event subscription: {error}"
        );
        if let Err(err) = pmt_config::webhook_subscriptions::remove(&self.scope, name) {
            tracing::warn!(
                platform = PLATFORM,
                route = name,
                "ended MCP event route not removed: {err}"
            );
        }
        let target = route.deliver_target();
        if !matches!(target, "log" | "origin") {
            let reason: String = error["message"]
                .as_str()
                .unwrap_or("no reason given")
                .chars()
                .take(MAX_NOTICE_CHARS)
                .collect();
            let notice = format!(
                "MCP server '{}' ended the subscription to '{}': {reason} (code {}). Subscribe again to keep receiving these events.",
                event.server, event.name, error["code"]
            );
            let link = self.link.lock().expect("link poisoned").clone();
            if let Some(link) = link
                && let Err(why) = link.deliver(target, &notice).await
            {
                tracing::warn!(
                    platform = PLATFORM,
                    route = name,
                    "termination notice to {target} failed: {why}"
                );
            }
        }
        status(200, &json!({"status": "terminated"}))
    }
}

fn verification(payload: &Value) -> HttpResponse {
    match payload["challenge"].as_str() {
        Some(challenge)
            if !challenge.is_empty() && challenge.chars().count() <= MAX_CHALLENGE_CHARS =>
        {
            status(200, &json!({"challenge": challenge}))
        }
        _ => status(400, &json!({"error": "invalid verification challenge"})),
    }
}
