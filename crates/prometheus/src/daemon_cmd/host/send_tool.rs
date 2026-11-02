//! The `send_message` tool of daemon sessions (spec: agent-interop,
//! `send_message` tool): sends through the gateway's outbox after the
//! destination is authorized (home channel, allowed chat or a chat with a
//! session, and the relay's egress check for relayed platforms; any error
//! refuses), mirrors the sent message into the target chat's session, and
//! attaches only files the session may read.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use pmt_agent::tools::messaging::NO_GATEWAY;
use pmt_agent::tools::{MessagingPort, MessagingPorts, ReadCheck};
use pmt_agent::turn::model::BoxFuture;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_gateway::messaging::{Gateway, target};
use pmt_tools::ToolResult;
use serde_json::Value;

/// The gateway, once the daemon started it.
pub(crate) type GatewayCell = Arc<OnceLock<Gateway>>;

/// Builds the ports of one profile's sessions.
pub(crate) struct GatewayPorts {
    pub(crate) gateway: GatewayCell,
    pub(crate) profile: String,
}

impl MessagingPorts for GatewayPorts {
    fn port(&self, session: SessionId, may_read: ReadCheck) -> Arc<dyn MessagingPort> {
        let _ = session;
        Arc::new(GatewayPort {
            gateway: Arc::clone(&self.gateway),
            profile: self.profile.clone(),
            may_read,
        })
    }
}

struct GatewayPort {
    gateway: GatewayCell,
    profile: String,
    may_read: ReadCheck,
}

impl MessagingPort for GatewayPort {
    fn call(&self, session: SessionId, arguments: Value) -> BoxFuture<'_, ToolResult> {
        Box::pin(async move {
            let Some(gateway) = self.gateway.get() else {
                return ToolResult::error(NO_GATEWAY);
            };
            match self.run(gateway, session, &arguments).await {
                Ok(text) => ToolResult::ok(text),
                Err(err) => ToolResult::error(err.message()),
            }
        })
    }
}

fn argument<'a>(arguments: &'a Value, key: &str) -> Option<&'a str> {
    arguments[key]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn required<'a>(arguments: &'a Value, key: &str, action: &str) -> Result<&'a str, Error> {
    argument(arguments, key).ok_or_else(|| {
        Error::new(
            &codes::CLI_USAGE,
            format!("send_message {action} needs '{key}'"),
        )
    })
}

impl GatewayPort {
    async fn run(
        &self,
        gateway: &Gateway,
        session: SessionId,
        arguments: &Value,
    ) -> Result<String, Error> {
        let action = argument(arguments, "action").unwrap_or("send");
        let profile = &self.profile;
        match action {
            "list" => {
                let targets = gateway.known_targets(profile, argument(arguments, "target"))?;
                if targets.is_empty() {
                    return Ok(
                        "No reachable chats: no home channel, allowed chat or chat with a session."
                            .to_owned(),
                    );
                }
                Ok(targets
                    .iter()
                    .map(|t| format!("{} ({})", t.target, t.kind))
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "send" => {
                let to = gateway
                    .resolve_target(profile, required(arguments, "target", action)?)
                    .await?;
                let (text, attachments) = target::media(required(arguments, "message", action)?);
                for file in &attachments {
                    let path = Path::new(&file.path);
                    if !path.is_absolute() || !path.is_file() || !(self.may_read)(path) {
                        return Err(Error::new(
                            &codes::GATEWAY_TARGET_REFUSED,
                            format!("MEDIA:{} is not a file this session may read", file.path),
                        ));
                    }
                }
                gateway.authorize_destination(profile, &to).await?;
                let entry = gateway.enqueue(profile, &to, &text, &attachments)?;
                gateway.mirror(profile, &to, &text, Some(&session.to_string()));
                Ok(format!(
                    "Sent to {} (outbox entry {}).",
                    to.label(),
                    entry.id
                ))
            }
            "react" | "unreact" => {
                let to = gateway
                    .resolve_target(profile, required(arguments, "target", action)?)
                    .await?;
                let emoji = required(arguments, "emoji", action)?;
                gateway.authorize_destination(profile, &to).await?;
                gateway
                    .react(
                        profile,
                        &to,
                        argument(arguments, "message_id"),
                        emoji,
                        action == "unreact",
                    )
                    .await?;
                Ok(format!(
                    "{} {emoji} in {}.",
                    if action == "react" {
                        "Reacted"
                    } else {
                        "Removed"
                    },
                    to.label()
                ))
            }
            other => Err(Error::new(
                &codes::CLI_USAGE,
                format!("unknown send_message action '{other}' (send, list, react, unreact)"),
            )),
        }
    }
}
