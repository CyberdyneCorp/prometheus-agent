//! The realtime full-duplex voice session (spec: voice-mode, Realtime
//! full-duplex voice chat mode): the desktop's WebRTC offer goes to the
//! daemon, which creates the call with the profile's key and answers with
//! the provider's SDP and the call id only, so the key never reaches the
//! client. The session it creates speaks with `voice.realtime.voice` and
//! offers one tool, [`AGENT_TOOL`], through which every substantive
//! request goes to the Prometheus agent.

use pmt_config::schema::RealtimeVoiceConfig;
use reqwest::header::{CONTENT_TYPE, LOCATION};
use serde_json::{Value, json};

use crate::MediaSession;
use crate::http::{Multipart, status_error, transport};

/// The providers a realtime session can use.
pub const PROVIDERS: &[&str] = &["openai"];

/// The one tool of the realtime session: the agent does the work.
pub const AGENT_TOOL: &str = "prometheus_agent";

/// What the voice layer is told, before `voice.realtime.instructions`.
pub const INSTRUCTIONS: &str = "You are the voice of the Prometheus agent. \
Hand every substantive request (anything about files, code, the web, memory, \
schedules or tasks, or anything you would have to look up or do) to the \
prometheus_agent tool with the user's request in full, then speak its result \
briefly and naturally. Only small talk and clarifying questions are yours to \
answer. Never claim to have done something the tool did not report.";

/// A created realtime call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealtimeCall {
    /// The provider's call id.
    pub call_id: String,
    /// The provider's SDP answer.
    pub answer: String,
    /// The provider.
    pub provider: String,
    /// The model.
    pub model: String,
    /// How the call bills.
    pub billing_source: String,
}

/// The session the provider creates: the voice, the instructions and the
/// one delegated tool.
#[must_use]
pub fn session(config: &RealtimeVoiceConfig) -> Value {
    let instructions = match &config.instructions {
        Some(extra) if !extra.trim().is_empty() => format!("{INSTRUCTIONS}\n\n{extra}"),
        _ => INSTRUCTIONS.to_owned(),
    };
    json!({
        "type": "realtime",
        "model": config.model,
        "instructions": instructions,
        "audio": {"output": {"voice": config.voice}},
        "tools": [{
            "type": "function",
            "name": AGENT_TOOL,
            "description": "Ask the Prometheus agent to do or answer something. It works with \
                the user's files, tools, memory and permissions and returns its answer.",
            "parameters": {
                "type": "object",
                "properties": {"request": {
                    "type": "string",
                    "description": "The user's request, complete and in their words.",
                }},
                "required": ["request"],
            },
        }],
        "tool_choice": "auto",
    })
}

impl MediaSession {
    /// Creates a realtime call for the client's SDP `offer`.
    ///
    /// # Errors
    ///
    /// An unsupported provider, a missing key, or the provider's refusal.
    pub async fn realtime_call(
        &self,
        config: &RealtimeVoiceConfig,
        offer: &str,
    ) -> Result<RealtimeCall, String> {
        if !PROVIDERS.contains(&config.provider.as_str()) {
            return Err(format!(
                "voice.realtime.provider '{}' is not supported (supported: {})",
                config.provider,
                PROVIDERS.join(", ")
            ));
        }
        let access = self
            .setup()
            .credentials
            .key(&config.provider, "OPENAI_API_KEY")?;
        let token = access.bearer()?.expose().to_owned();
        let (kind, body) = Multipart::new()
            .text("sdp", offer)
            .text("session", &session(config).to_string())
            .finish();
        let url = format!("{}/realtime/calls", config.base_url.trim_end_matches('/'));
        let reply = self
            .setup()
            .client
            .post(url)
            .bearer_auth(token)
            .header(CONTENT_TYPE, kind)
            .body(body)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let status = reply.status();
        let location = reply
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let answer = reply.bytes().await.map_err(|e| transport(&e))?;
        if !status.is_success() {
            return Err(status_error(status.as_u16(), &answer));
        }
        let call_id = location
            .as_deref()
            .and_then(|l| l.rsplit('/').next())
            .filter(|id| !id.is_empty())
            .ok_or("the provider answered without a call id")?
            .to_owned();
        Ok(RealtimeCall {
            call_id,
            answer: String::from_utf8_lossy(&answer).into_owned(),
            provider: config.provider.clone(),
            model: config.model.clone(),
            billing_source: access.billing_source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_offers_one_agent_tool_and_appends_the_instructions() {
        let config = RealtimeVoiceConfig {
            instructions: Some("Be terse.".to_owned()),
            ..RealtimeVoiceConfig::default()
        };
        let session = session(&config);
        let tools = session["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], AGENT_TOOL);
        assert_eq!(session["audio"]["output"]["voice"], "marin");
        let instructions = session["instructions"].as_str().unwrap();
        assert!(instructions.starts_with(INSTRUCTIONS) && instructions.ends_with("Be terse."));
    }
}
