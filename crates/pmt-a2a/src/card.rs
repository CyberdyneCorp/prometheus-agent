//! The Agent Card (spec: agent-interop, Agent Card), served at
//! `/.well-known/agent-card.json` and the legacy `/.well-known/agent.json`.
//! The card is v1.0 (`supportedInterfaces`, proto-JSON security schemes)
//! and also carries the pre-1.0 connection fields (`url`,
//! `protocolVersion`, `preferredTransport`) that v0.3 readers use; v1.0
//! readers drop them when `supportedInterfaces` is present.

use serde_json::{Value, json};

use crate::wire::PROTOCOL_VERSION;

/// One advertised skill (a toolset).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// The toolset id.
    pub id: String,
    /// Its display name.
    pub name: String,
    /// What it does.
    pub description: String,
}

/// What a card describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardInput {
    /// The agent's name.
    pub name: String,
    /// What it does.
    pub description: String,
    /// The callable JSON-RPC URL.
    pub url: String,
    /// The software version.
    pub version: String,
    /// Whether callers must send a bearer token.
    pub bearer: bool,
    /// The skills, one per advertised toolset.
    pub skills: Vec<Skill>,
}

/// The card's JSON.
#[must_use]
pub fn card(input: &CardInput) -> Value {
    let skills: Vec<Value> = input
        .skills
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "name": s.name,
                "description": s.description,
                "tags": [s.id],
            })
        })
        .collect();
    let mut card = json!({
        "name": input.name,
        "description": input.description,
        "version": input.version,
        "supportedInterfaces": [{
            "url": input.url,
            "protocolBinding": "JSONRPC",
            "protocolVersion": PROTOCOL_VERSION,
        }],
        "url": input.url,
        "protocolVersion": PROTOCOL_VERSION,
        "preferredTransport": "JSONRPC",
        "provider": {"organization": "Prometheus", "url": "https://github.com/CyberdyneCorp/prometheus-agent"},
        "capabilities": {"streaming": true, "pushNotifications": true},
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "skills": skills,
    });
    if input.bearer {
        card["securitySchemes"] = json!({
            "bearer": {
                "httpAuthSecurityScheme": {"scheme": "bearer"},
                "type": "http",
                "scheme": "bearer",
            }
        });
        card["securityRequirements"] = json!([{"schemes": {"bearer": {"list": []}}}]);
        card["security"] = json!([{"bearer": []}]);
    }
    card
}

/// What a fetched card says, read leniently (v1.0 or pre-1.0).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CardSummary {
    /// The agent's name.
    pub name: String,
    /// What it does.
    pub description: String,
    /// The JSON-RPC URL to call (`None`: the card names none).
    pub url: Option<String>,
    /// The protocol version of that interface.
    pub protocol_version: Option<String>,
    /// Whether it streams.
    pub streaming: bool,
    /// Whether it sends push notifications.
    pub push: bool,
    /// Its skills: id, name, description.
    pub skills: Vec<Skill>,
    /// The security schemes it names.
    pub auth: Vec<String>,
}

impl CardSummary {
    /// Reads a card.
    #[must_use]
    pub fn of(card: &Value) -> Self {
        let text = |v: &Value| v.as_str().unwrap_or_default().to_owned();
        let jsonrpc = card["supportedInterfaces"].as_array().and_then(|list| {
            list.iter()
                .find(|i| i["protocolBinding"].as_str().is_none_or(|b| b == "JSONRPC"))
        });
        let url = jsonrpc
            .and_then(|i| i["url"].as_str())
            .or_else(|| card["url"].as_str())
            .map(str::to_owned);
        let protocol_version = jsonrpc
            .and_then(|i| i["protocolVersion"].as_str())
            .or_else(|| card["protocolVersion"].as_str())
            .map(str::to_owned);
        let skills = card["skills"]
            .as_array()
            .map(|list| {
                list.iter()
                    .map(|s| Skill {
                        id: text(&s["id"]),
                        name: text(&s["name"]),
                        description: text(&s["description"]),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let auth = card["securitySchemes"]
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        Self {
            name: text(&card["name"]),
            description: text(&card["description"]),
            url,
            protocol_version,
            streaming: card["capabilities"]["streaming"].as_bool().unwrap_or(false),
            push: card["capabilities"]["pushNotifications"]
                .as_bool()
                .unwrap_or(false),
            skills,
            auth,
        }
    }

    /// Whether the agent speaks only the pre-1.0 protocol.
    #[must_use]
    pub fn legacy(&self) -> bool {
        self.protocol_version
            .as_deref()
            .is_some_and(|v| v.starts_with("0."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_reads_back() {
        let input = CardInput {
            name: "host-default".into(),
            description: "d".into(),
            url: "https://agent.example.com/a2a".into(),
            version: "0.1.0".into(),
            bearer: true,
            skills: vec![Skill {
                id: "web".into(),
                name: "Web".into(),
                description: "Search".into(),
            }],
        };
        let summary = CardSummary::of(&card(&input));
        assert_eq!(
            summary.url.as_deref(),
            Some("https://agent.example.com/a2a")
        );
        assert_eq!(summary.skills.len(), 1);
        assert_eq!(summary.auth, ["bearer"]);
        assert!(!summary.legacy());
        let legacy = json!({"name": "x", "url": "http://h/", "protocolVersion": "0.3.0"});
        assert!(CardSummary::of(&legacy).legacy());
    }
}
