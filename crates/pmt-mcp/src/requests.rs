//! What a server asks of Prometheus: sampling and elicitation (spec:
//! plugin-system, MCP sampling; MCP elicitation).
//!
//! Both belong to the session whose tool call is in flight on the server
//! (the newest one); with none, sampling is refused and elicitation
//! declined at once. Sampling is on by default only for `trust = "full"`
//! servers and runs within the server's limits: a sliding-minute request
//! rate, tool rounds, a model allow list, a capped answer size and a
//! timeout; a limit that is exceeded is a protocol error naming it, and no
//! model call is made. Each call is charged to the session's ledger as
//! `mcp:<server>`. Form elicitations go to the session's person (answer
//! once or decline, whatever the approval mode); URL elicitations are
//! declined as unsupported.

#![allow(deprecated)] // sampling is deprecated upstream (SEP-2577) but specified here

use std::collections::VecDeque;
use std::sync::PoisonError;
use std::time::Duration;

use pmt_config::schema::{McpElicitationConfig, McpSamplingConfig, McpTrust};
use rmcp::ErrorData;
use rmcp::model::{
    CreateMessageRequestParams, CreateMessageResult, ElicitRequestParams, Role, SamplingMessage,
    SamplingMessageContentBlock,
};
use tokio::time::Instant;

use crate::host::{ElicitAnswer, Elicitation, Sampling};
use crate::server::Server;

/// The sliding window of `max_rpm`.
const MINUTE: Duration = Duration::from_mins(1);

/// A server's sampling limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamplingPolicy {
    /// Requests are answered at all.
    pub enabled: bool,
    /// The model answering instead of the session's.
    pub model: Option<String>,
    /// When not empty, `model` must be one of these.
    pub allowed_models: Vec<String>,
    /// Requested answer sizes are capped at this.
    pub max_tokens_cap: u32,
    /// Bound on one call.
    pub timeout: Duration,
    /// Requests per sliding minute.
    pub max_rpm: u32,
    /// Tool rounds of one exchange.
    pub max_tool_rounds: u32,
}

impl Default for SamplingPolicy {
    fn default() -> Self {
        Self::of(&McpSamplingConfig::default(), McpTrust::Untrusted)
    }
}

impl SamplingPolicy {
    /// The policy of `[mcp.servers.<name>.sampling]` for a server trusted
    /// `trust`.
    #[must_use]
    pub fn of(config: &McpSamplingConfig, trust: McpTrust) -> Self {
        Self {
            enabled: config.enabled.unwrap_or(trust == McpTrust::Full),
            model: config.model.clone(),
            allowed_models: config.allowed_models.clone(),
            max_tokens_cap: config.max_tokens_cap,
            timeout: Duration::from_secs(config.timeout_secs),
            max_rpm: config.max_rpm,
            max_tool_rounds: config.max_tool_rounds,
        }
    }
}

/// A server's elicitation settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElicitationPolicy {
    /// Form requests reach the person.
    pub enabled: bool,
    /// Declined when unanswered this long.
    pub timeout: Duration,
}

impl Default for ElicitationPolicy {
    fn default() -> Self {
        Self::of(&McpElicitationConfig::default())
    }
}

impl ElicitationPolicy {
    /// The policy of `[mcp.servers.<name>.elicitation]`.
    #[must_use]
    pub fn of(config: &McpElicitationConfig) -> Self {
        Self {
            enabled: config.enabled,
            timeout: Duration::from_secs(config.timeout_secs),
        }
    }
}

/// When the last minute's sampling requests were admitted.
#[derive(Debug, Default)]
pub struct Window(VecDeque<Instant>);

impl Window {
    /// Admits one request at `now` unless `max` were admitted within the
    /// last minute.
    fn admit(&mut self, now: Instant, max: u32) -> bool {
        while self
            .0
            .front()
            .is_some_and(|at| now.duration_since(*at) >= MINUTE)
        {
            self.0.pop_front();
        }
        if self.0.len() >= usize::try_from(max).unwrap_or(usize::MAX) {
            return false;
        }
        self.0.push_back(now);
        true
    }
}

fn refused(server: &str, limit: &str) -> ErrorData {
    ErrorData::invalid_request(
        format!("MCP server '{server}': sampling refused: {limit}"),
        None,
    )
}

impl Server {
    /// Answers `sampling/createMessage`.
    pub(crate) async fn sample(
        &self,
        params: CreateMessageRequestParams,
    ) -> Result<CreateMessageResult, ErrorData> {
        let policy = &self.settings().sampling;
        let name = self.name();
        if !policy.enabled {
            return Err(refused(name, "sampling is disabled for this server"));
        }
        let Some(session) = self.caller() else {
            return Err(refused(
                name,
                "no session: no tool call of this server is in flight",
            ));
        };
        let request = self.sampling_request(&params)?;
        let admitted = self
            .sampling_window
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .admit(Instant::now(), policy.max_rpm);
        if !admitted {
            return Err(refused(
                name,
                &format!("rate limit exceeded (max_rpm = {})", policy.max_rpm),
            ));
        }
        let host = self.host().sample(&session, request);
        let sampled = match tokio::time::timeout(policy.timeout, host).await {
            Err(_) => {
                return Err(refused(
                    name,
                    &format!(
                        "no answer within timeout_secs = {}",
                        policy.timeout.as_secs()
                    ),
                ));
            }
            Ok(Err(why)) => return Err(ErrorData::internal_error(why, None)),
            Ok(Ok(sampled)) => sampled,
        };
        let mut result =
            CreateMessageResult::new(SamplingMessage::assistant_text(sampled.text), sampled.model);
        result.stop_reason = Some("endTurn".to_owned());
        Ok(result)
    }

    /// The host request of `params`, or the limit it exceeds.
    fn sampling_request(&self, params: &CreateMessageRequestParams) -> Result<Sampling, ErrorData> {
        let policy = &self.settings().sampling;
        let name = self.name();
        let rounds = params
            .messages
            .iter()
            .filter(|m| {
                m.role == Role::Assistant
                    && m.content
                        .clone()
                        .into_vec()
                        .iter()
                        .any(|b| matches!(b, SamplingMessageContentBlock::ToolUse(_)))
            })
            .count();
        if rounds > usize::try_from(policy.max_tool_rounds).unwrap_or(usize::MAX) {
            return Err(refused(
                name,
                &format!(
                    "too many tool rounds (max_tool_rounds = {})",
                    policy.max_tool_rounds
                ),
            ));
        }
        if let Some(model) = &policy.model
            && !policy.allowed_models.is_empty()
            && !policy.allowed_models.contains(model)
        {
            return Err(refused(
                name,
                &format!("model '{model}' is not in allowed_models"),
            ));
        }
        Ok(Sampling {
            server: name.to_owned(),
            system: params.system_prompt.clone(),
            messages: params.messages.iter().map(message).collect(),
            model: policy.model.clone(),
            max_tokens: params.max_tokens.min(policy.max_tokens_cap),
        })
    }

    /// Answers `elicitation/create`.
    pub(crate) async fn elicit(&self, request: ElicitRequestParams) -> ElicitAnswer {
        let policy = self.settings().elicitation;
        if !policy.enabled {
            return ElicitAnswer::Decline;
        }
        let ElicitRequestParams::FormElicitationParams {
            message,
            requested_schema,
            ..
        } = request
        else {
            tracing::info!(server = %self.name(), "URL-mode elicitation declined: unsupported");
            return ElicitAnswer::Decline;
        };
        let Some(session) = self.caller() else {
            return ElicitAnswer::Decline;
        };
        let elicitation = Elicitation {
            server: self.name().to_owned(),
            message: crate::sanitize::strip_tags(&message),
            schema: crate::sanitize::value_without_tags(
                &serde_json::to_value(requested_schema).unwrap_or_default(),
            ),
        };
        let asked = self.host().elicit(&session, elicitation);
        tokio::time::timeout(policy.timeout, asked)
            .await
            .unwrap_or(ElicitAnswer::Decline)
    }
}

/// One sampling message as `(role, text)`.
fn message(message: &SamplingMessage) -> (String, String) {
    let role = match message.role {
        Role::Assistant => "assistant",
        Role::User => "user",
    };
    let parts: Vec<String> = message
        .content
        .clone()
        .into_vec()
        .iter()
        .map(|block| match block {
            SamplingMessageContentBlock::Text(text) => crate::sanitize::strip_tags(&text.text),
            SamplingMessageContentBlock::Image(_) => "[image]".to_owned(),
            SamplingMessageContentBlock::Audio(_) => "[audio]".to_owned(),
            SamplingMessageContentBlock::ToolUse(tool) => format!("[tool call {}]", tool.name),
            SamplingMessageContentBlock::ToolResult(_) => "[tool result]".to_owned(),
            _ => "[content]".to_owned(),
        })
        .collect();
    (role.to_owned(), parts.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_admits_max_per_sliding_minute() {
        let mut window = Window::default();
        let start = Instant::now();
        assert!((0..10).all(|_| window.admit(start, 10)));
        assert!(!window.admit(start + Duration::from_secs(59), 10));
        assert!(window.admit(start + MINUTE, 10));
    }

    #[test]
    fn sampling_is_on_by_default_only_for_trusted_servers() {
        let config = McpSamplingConfig::default();
        assert!(SamplingPolicy::of(&config, McpTrust::Full).enabled);
        assert!(!SamplingPolicy::of(&config, McpTrust::Untrusted).enabled);
    }
}
